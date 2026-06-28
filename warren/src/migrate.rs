use crate::db::Db;
use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, DatabaseBackend, FromQueryResult, Statement};
use std::collections::HashSet;
use std::time::Instant;

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

const SCHEMA_MIGRATIONS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version VARCHAR(255) PRIMARY KEY,
    description VARCHAR(255) NOT NULL,
    type VARCHAR(20) NOT NULL,
    applied INT NOT NULL DEFAULT 0,
    hash VARCHAR(255) NOT NULL,
    execution_time INT NOT NULL DEFAULT 0,
    success INT NOT NULL DEFAULT 0,
    error TEXT NULL,
    error_code TEXT NULL,
    installed_by VARCHAR(255) NULL,
    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
)
"#;

pub async fn run(db: &Db) -> Result<()> {
    db.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        SCHEMA_MIGRATIONS_DDL,
    ))
    .await
    .context("creating schema_migrations table")?;

    let applied = load_applied(db)
        .await
        .context("loading applied migrations")?;
    let mut any_applied = false;

    for (version, description, sql) in MIGRATIONS.iter() {
        if applied.contains(*version) {
            log::info!("migration {version} already applied");
            continue;
        }
        log::info!("applying migration {version} ({description})");
        any_applied = true;
        let started = Instant::now();
        let mut exec_result: Result<(), sea_orm::DbErr> = Ok(());
        for stmt in split_sql(sql) {
            if let Err(e) = db
                .execute(Statement::from_string(DatabaseBackend::Postgres, stmt))
                .await
            {
                exec_result = Err(e);
                break;
            }
        }
        match exec_result {
            Ok(_) => {
                let elapsed_ms = started.elapsed().as_millis() as i32;
                record_applied(db, version, description, sql, elapsed_ms, true, None)
                    .await
                    .with_context(|| format!("recording migration {version}"))?;
                log::info!("applied migration {version} in {elapsed_ms}ms");
            }
            Err(e) => {
                let elapsed_ms = started.elapsed().as_millis() as i32;
                let msg = format!("{e:?}");
                let code = format!("{}", e);
                let _ = record_applied(
                    db,
                    version,
                    description,
                    sql,
                    elapsed_ms,
                    false,
                    Some((msg.as_str(), code.as_str())),
                )
                .await;
                return Err(anyhow::Error::from(e).context(format!("applying migration {version}")));
            }
        }
    }

    if !any_applied {
        log::info!("no pending migrations");
    }
    Ok(())
}

async fn load_applied(db: &Db) -> Result<HashSet<String>> {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT version FROM schema_migrations WHERE success = 1",
        ))
        .await
        .context("querying schema_migrations")?;

    #[derive(FromQueryResult)]
    struct Row {
        version: String,
    }

    rows.into_iter()
        .map(|r| Row::from_query_result(&r, "").map(|row| row.version))
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

async fn record_applied(
    db: &Db,
    version: &str,
    description: &str,
    sql: &str,
    elapsed_ms: i32,
    success: bool,
    error: Option<(&str, &str)>,
) -> Result<()> {
    let hash = compute_hash(sql);
    let success_i = if success { 1 } else { 0 };
    let (err_col, code_col) = match error {
        Some((msg, code)) => (
            format!("'{}'", msg.replace('\'', "''")),
            format!("'{}'", code.replace('\'', "''")),
        ),
        None => ("NULL".to_string(), "NULL".to_string()),
    };
    let stmt = format!(
        "INSERT INTO schema_migrations \
         (version, description, type, applied, hash, execution_time, success, error, error_code, installed_by) \
         VALUES ('{}', '{}', 'sql', 1, '{}', {}, {}, {}, {}, 'warren') \
         ON CONFLICT (version) DO UPDATE SET \
            description = EXCLUDED.description, \
            type = EXCLUDED.type, \
            applied = EXCLUDED.applied, \
            hash = EXCLUDED.hash, \
            execution_time = EXCLUDED.execution_time, \
            success = EXCLUDED.success, \
            error = EXCLUDED.error, \
            error_code = EXCLUDED.error_code, \
            installed_on = CURRENT_TIMESTAMP",
        escape(version),
        escape(description),
        hash,
        elapsed_ms,
        success_i,
        err_col,
        code_col,
    );
    db.execute(Statement::from_string(DatabaseBackend::Postgres, stmt))
        .await?;
    Ok(())
}

fn escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn split_sql(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") || trimmed.is_empty() {
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
        if line.trim_end().ends_with(';') {
            let stmt = buf.trim().to_string();
            if !stmt.is_empty() {
                out.push(stmt);
            }
            buf.clear();
        }
    }
    let tail = buf.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn compute_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())
}
