//! Enforces the README policy: "warren/src/entity/*.rs — schema
//! source of truth. Edit an entity → `warren dump-schema` → `atlas
//! migrate diff` → commit the new migration." Catches the class of
//! bug where a hand-written migration drifts from the entity types.
//!
//! Skips gracefully if `atlas` or `warren` isn't on PATH or if no DB
//! is reachable. Run explicitly via `cargo test -p warren
//! --test schema_drift -- --ignored` from a checkout that has both
//! binaries built and a DB available.

use std::process::Command;

fn has(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok()
}

fn admin_url() -> String {
    std::env::var("WARREN_DRIFT_ADMIN_URL")
        .unwrap_or_else(|_| "postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable".into())
}

fn test_db_name() -> String {
    format!(
        "warren_drift_{}_{}",
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

#[test]
#[ignore]
fn entity_schema_matches_migrations() {
    if !has("atlas") || !has("warren") {
        eprintln!("skip: atlas or warren binary not on PATH");
        return;
    }

    let db = test_db_name();
    let target_url = format!("postgres://postgres@127.0.0.1:5432/{db}?sslmode=disable");
    let dev_url = format!("postgres://postgres@127.0.0.1:5432/{db}_dev?sslmode=disable");

    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}\""))
        .expect("create test db");
    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}_dev\""))
        .expect("create dev db");

    let result = (|| -> Result<(), String> {
        let apply = Command::new("atlas")
            .args([
                "migrate",
                "apply",
                "--dir",
                "file:///workdir/warren/migrations_atlas",
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

        let dump = Command::new("warren")
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
        std::fs::write("/tmp/warren-drift-desired.sql", &desired)
            .map_err(|e| format!("write /tmp: {e}"))?;

        let diff = Command::new("atlas")
            .args([
                "schema",
                "diff",
                "--from",
                &target_url,
                "--to",
                "file:///tmp/warren-drift-desired.sql",
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
            return Err(format!(
                "entity schema drifted from migrations. Run `warren dump-schema` + \
                 `atlas migrate diff` to regenerate, or fix the entity. Drift:\n{}",
                lines.join("\n")
            ));
        }
        Ok(())
    })();

    let _ = psql_exec(&admin_url(), &format!("DROP DATABASE IF EXISTS \"{db}\""));
    let _ = psql_exec(&admin_url(), &format!("DROP DATABASE IF EXISTS \"{db}_dev\""));

    if let Err(e) = result {
        panic!("{e}");
    }
}