//! Populated-DB mirror of `tests/schema_drift.rs`. Adds two safety
//! nets that the empty-DB drift test cannot catch:
//!
//! 1. **NOT NULL without DEFAULT**. The schema-drift test applies
//!    migrations to an empty DB, then `atlas schema diff`s the
//!    result against `warren dump-schema`. A `NOT NULL` column
//!    without a server-side `DEFAULT` is accepted by Postgres on an
//!    empty table — the failure only surfaces once production data
//!    exists. This was the bug that broke `scheduled_prompts.scope`
//!    on the live cluster.
//! 2. **Diff between desired.sql and a populated DB.** Catches any
//!    future entity/migration divergence that only manifests when
//!    rows exist.
//!
//! Both tests are `#[ignore]` (need atlas + Postgres + a built
//! `warren` binary) and reuse the skip-when-missing-binary pattern
//! from `tests/schema_drift.rs`. Run from a checkout with all three
//! via `cargo test -p warren --test schema_drift_populated
//! -- --ignored --nocapture`.

use std::path::PathBuf;
use std::process::Command;

fn warren_bin() -> Option<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_warren").map(PathBuf::from)
}

fn migrations_dir_url() -> String {
    format!("file://{}/migrations_atlas", env!("CARGO_MANIFEST_DIR"))
}

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok()
}

fn admin_url() -> String {
    std::env::var("WARREN_DRIFT_ADMIN_URL")
        .unwrap_or_else(|_| "postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable".into())
}

fn test_db_name(label: &str) -> String {
    format!(
        "warren_pop_{label}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn psql_exec(url: &str, sql: &str) -> Result<String, String> {
    let out = Command::new("psql")
        .args([url, "-tAc", sql])
        .output()
        .map_err(|e| format!("psql spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "psql failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn drop_db_set(db: &str) {
    let _ = psql_exec(&admin_url(), &format!("DROP DATABASE IF EXISTS \"{db}\""));
    let _ = psql_exec(
        &admin_url(),
        &format!("DROP DATABASE IF EXISTS \"{db}_dev\""),
    );
}

/// Hand-rolled minimal-row insert per table. The test only needs
/// SOMETHING in each table — values are throwaway but must satisfy
/// every NOT NULL constraint and any UNIQUE constraint. FK targets
/// for dependent tables are inserted in the same statement via a
/// CTE.
///
/// Kept short and explicit on purpose: a generated from
/// `information_schema` approach turned into a yak shave (UNIQUE
/// detection, type-width inference, FK ordering). Eight tables is
/// cheaper to maintain by hand than to express the schema-aware
/// version robustly.
fn min_insert_sql(table: &str) -> &'static str {
    match table {
        "agents" => "INSERT INTO agents (name, class, model, authtoken) \
                     VALUES (gen_random_uuid()::text, 'x', 'x', gen_random_uuid()::text)",
        "channels" => "INSERT INTO channels (sender_class, receiver_class, \
                       sender_kind, receiver_kind, description, \
                       requires_request_approval, requires_response_approval, \
                       enabled) \
                       VALUES ('x', 'x', NULL, NULL, 'x', true, true, true)",
        "requests" => "INSERT INTO requests (target_class, target_type, \
                       payload, response, status, sender_agent_id, \
                       claimed_by, claimed_at) \
                       VALUES ('x', NULL, 'x', NULL, 0, NULL, NULL, NULL)",
        "admin_sessions" => "INSERT INTO admin_sessions (token, expires_at) \
                             VALUES (gen_random_uuid()::text, now() + interval '1 day')",
        "agent_events" => "WITH _p AS (INSERT INTO agents \
                              (name, class, model, authtoken) \
                              VALUES (gen_random_uuid()::text, 'x', 'x', gen_random_uuid()::text) \
                              RETURNING id) \
                           INSERT INTO agent_events (id, agent_id, seq, ts, kind, payload) \
                           SELECT gen_random_uuid(), id, 1, now(), 'x', '{}'::jsonb FROM _p",
        "agent_forgejo_configs" => "WITH _p AS (INSERT INTO agents \
                                        (name, class, model, authtoken) \
                                        VALUES (gen_random_uuid()::text, 'x', 'x', gen_random_uuid()::text) \
                                        RETURNING id) \
                                     INSERT INTO agent_forgejo_configs \
                                       (agent_id, forgejo_username, base_url, owner, \
                                        repo, access_token) \
                                     SELECT id, 'x', 'x', 'x', 'x', 'x' FROM _p",
        "scheduled_prompts" => "INSERT INTO scheduled_prompts \
                                  (name, prompt_text, interval_seconds, \
                                   target_class, target_kind, scope, \
                                   agent_id, ignore_inbox_state, \
                                   ignore_pending_forgejo_work) \
                                  VALUES (gen_random_uuid()::text, 'x', 60, 'x', NULL, 'team', \
                                          NULL, false, false)",
        "scheduled_prompt_runs" => {
            "WITH _p AS (INSERT INTO scheduled_prompts \
             (name, prompt_text, interval_seconds, target_class, \
              target_kind, scope, agent_id, ignore_inbox_state, \
              ignore_pending_forgejo_work) \
             VALUES (gen_random_uuid()::text, 'x', 60, 'x', NULL, 'team', NULL, false, false) \
             RETURNING id) \
         INSERT INTO scheduled_prompt_runs \
           (id, scheduled_prompt_id, agent_id, fired_at, finished_at, \
            outcome, skip_reason, prompt_id, outcome_error, \
            usage_weekly_pct, usage_session_pct, usage_context_pct) \
         SELECT gen_random_uuid(), id, NULL, now(), NULL, 'x', NULL, NULL, NULL, \
                NULL, NULL, NULL FROM _p"
        }
        _ => "",
    }
}

fn entity_tables() -> &'static [&'static str] {
    &[
        "agents",
        "channels",
        "requests",
        "admin_sessions",
        "agent_events",
        "agent_forgejo_configs",
        "scheduled_prompts",
        "scheduled_prompt_runs",
    ]
}

fn populate_all_tables(url: &str) -> Result<(), String> {
    for t in entity_tables() {
        let sql = min_insert_sql(t);
        if sql.is_empty() {
            return Err(format!("no min-insert defined for table {t}"));
        }
        psql_exec(url, sql).map_err(|e| format!("populate failed on table {t}: {e}"))?;
    }
    Ok(())
}

#[test]
#[ignore]
fn populated_schema_accepts_minimum_inserts() {
    if !has("atlas") {
        eprintln!("skip: atlas binary not on PATH");
        return;
    }
    let Some(_warren) = warren_bin() else {
        eprintln!("skip: CARGO_BIN_EXE_warren not set (run via `cargo test`)");
        return;
    };

    let db = test_db_name("min");
    let target_url = format!("postgres://postgres@127.0.0.1:5432/{db}?sslmode=disable");
    let dev_url = format!("postgres://postgres@127.0.0.1:5432/{db}_dev?sslmode=disable");

    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}\"")).expect("create test db");
    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}_dev\"")).expect("create dev db");

    let result: Result<(), String> = (|| {
        let apply = Command::new("atlas")
            .args([
                "migrate",
                "apply",
                "--dir",
                &migrations_dir_url(),
                "--url",
                &target_url,
            ])
            .output()
            .map_err(|e| format!("atlas migrate apply: {e}"))?;
        if !apply.status.success() {
            return Err(format!(
                "atlas migrate apply failed: {}",
                String::from_utf8_lossy(&apply.stderr)
            ));
        }

        match populate_all_tables(&target_url) {
            Ok(()) => Ok(()),
            Err(e) => {
                let help = " — add `#[sea_orm(default_value = \"...\")]` to the \
                            entity field (a server-side DEFAULT lets new rows \
                            backfill without supplying a value).";
                Err(format!("{e}{help}"))
            }
        }
    })();

    let _ = dev_url;
    drop_db_set(&db);
    if let Err(e) = result {
        panic!("{e}");
    }
}

#[test]
#[ignore]
fn desired_schema_lands_on_populated_db() {
    if !has("atlas") {
        eprintln!("skip: atlas binary not on PATH");
        return;
    }
    let Some(warren) = warren_bin() else {
        eprintln!("skip: CARGO_BIN_EXE_warren not set (run via `cargo test`)");
        return;
    };

    let db = test_db_name("diff");
    let target_url = format!("postgres://postgres@127.0.0.1:5432/{db}?sslmode=disable");
    let dev_url = format!("postgres://postgres@127.0.0.1:5432/{db}_dev?sslmode=disable");

    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}\"")).expect("create test db");
    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}_dev\"")).expect("create dev db");

    let result: Result<(), String> = (|| {
        let apply = Command::new("atlas")
            .args([
                "migrate",
                "apply",
                "--dir",
                &migrations_dir_url(),
                "--url",
                &target_url,
            ])
            .output()
            .map_err(|e| format!("atlas migrate apply: {e}"))?;
        if !apply.status.success() {
            return Err(format!(
                "atlas migrate apply failed: {}",
                String::from_utf8_lossy(&apply.stderr)
            ));
        }

        populate_all_tables(&target_url).map_err(|e| format!("populate: {e}"))?;

        let dump = Command::new(&warren)
            .args(["dump-schema"])
            .output()
            .map_err(|e| format!("warren dump-schema: {e}"))?;
        if !dump.status.success() {
            return Err(format!(
                "warren dump-schema failed: {}",
                String::from_utf8_lossy(&dump.stderr)
            ));
        }
        let desired = String::from_utf8_lossy(&dump.stdout).to_string();
        let desired_path = "/tmp/warren-drift-pop-desired.sql";
        std::fs::write(desired_path, &desired).map_err(|e| format!("write /tmp: {e}"))?;

        let diff = Command::new("atlas")
            .args([
                "schema",
                "diff",
                "--from",
                &target_url,
                "--to",
                &format!("file://{desired_path}"),
                "--dev-url",
                &dev_url,
            ])
            .output()
            .map_err(|e| format!("atlas schema diff: {e}"))?;
        let diff_out = String::from_utf8_lossy(&diff.stdout).to_string();
        let lines: Vec<&str> = diff_out
            .lines()
            .filter(|l| {
                let trimmed = l.trim_start_matches("-- ").trim();
                !trimmed.is_empty()
                    && !trimmed.starts_with("Skipped")
                    && !trimmed.starts_with("Atlas Pro")
                    && !trimmed.starts_with("atlas login")
                    && !trimmed.starts_with("https://")
                    && !trimmed.starts_with("Get started")
                    && !trimmed.contains("atlas_schema_revisions")
            })
            .collect();
        if !lines.is_empty() {
            let help = "\n— entity schema drifted from migrations on a populated \
                        DB. Run `warren dump-schema` + `atlas migrate diff` to \
                        regenerate, or fix the entity. A `NOT NULL` column \
                        without a `DEFAULT` only surfaces here, not on empty \
                        tables.";
            return Err(format!("drift:\n{}{}", lines.join("\n"), help));
        }
        Ok(())
    })();

    drop_db_set(&db);
    if let Err(e) = result {
        panic!("{e}");
    }
}
