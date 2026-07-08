mod auth;
mod config;
mod db;
mod db_ops;
mod entity;
mod error;
mod ids;
mod models;
mod rabbit_adapter;
mod routes;
mod templates;

use anyhow::Context;
use axum::{
    http::{HeaderName, HeaderValue},
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use config::Config;
use db::Db;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::set_header::SetResponseHeaderLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Config,
    /// `rabbit-lib`'s server state: owns the live agent registry plus
    /// the trait adapters that satisfy `SessionStore` and `AuthBackend`
    /// against Warren's SeaORM data layer.
    pub live: Arc<rabbit_lib::server::ServerState>,
}

/// `FromRef` impl so handlers written against `Router<Arc<ServerState>>`
/// in `rabbit-lib` can be merged into Warren's `Router<AppState>`. The
/// lib's handlers extract `State<Arc<ServerState>>`; `FromRef` is the
/// canonical axum way to let a sub-state be pulled out of a larger one.
impl axum::extract::FromRef<AppState> for Arc<rabbit_lib::server::ServerState> {
    fn from_ref(s: &AppState) -> Self {
        s.live.clone()
    }
}

#[derive(Parser)]
#[command(name = "warren", about = "warren server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    Server,
    /// Run `atlas migrate apply` against $DATABASE_URL using warren/migrations_atlas.
    #[command(name = "applyMigrations")]
    ApplyMigrations,
    /// Emit CREATE TABLE SQL for every entity to stdout.
    DumpSchema,
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("panic: {info}\n{bt}");
        log::error!("panic: {info}");
        log::error!("backtrace:\n{bt}");
    }));
    if let Err(e) = simple_logger::init_with_env() {
        eprintln!("error: failed to initialize logger: {e:?}");
    }

    let cli = Cli::parse();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log_error_and_exit("failed to build tokio runtime", e.into());
        }
    };

    let result = runtime.block_on(async move {
        match cli.command {
            Command::Server => run_server().await,
            Command::ApplyMigrations => run_apply_migrations().await,
            Command::DumpSchema => run_dump_schema().await,
        }
    });

    if let Err(e) = result {
        log_error_and_exit("warren failed", e);
    }
}

fn log_error_and_exit(context: &str, e: anyhow::Error) -> ! {
    log::error!("{context}: {e:?}");
    log::debug!("{context} chain: {e:#?}");
    eprintln!("error: {context}: {e:?}");
    std::io::stderr().flush().ok();
    std::process::exit(1);
}

async fn run_server() -> anyhow::Result<()> {
    let cfg = Config::from_env().context("loading configuration")?;
    log::info!(
        "connecting to database at {}",
        redact_url(&cfg.database_url)
    );
    let db = db::connect(&cfg.database_url)
        .await
        .context("connecting to database")?;
    let state = AppState {
        db: db.clone(),
        config: cfg.clone(),
        live: rabbit_adapter::build_server_state(db, cfg.tui_cols, cfg.tui_rows),
    };
    let app = build_router(state);

    let addr: SocketAddr = cfg
        .bind_addr
        .parse()
        .context(format!("parsing BIND_ADDR {:?}", cfg.bind_addr))?;
    log::info!("warren listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context(format!("binding TCP listener on {addr}"))?;
    if let Err(e) = axum::serve(listener, app.into_make_service()).await {
        log::error!("server stopped with error: {e:?}");
        log::debug!("server failure chain: {e:#?}");
        return Err(anyhow::Error::from(e).context("running HTTP server"));
    }
    Ok(())
}

fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = &url[scheme_end + 3..];
        if let Some(at) = after.find('@') {
            return format!("{}://***:***@{}", &url[..scheme_end], &after[at + 1..]);
        }
    }
    url.to_string()
}

async fn run_apply_migrations() -> anyhow::Result<()> {
    let cfg = Config::from_env_for_migrations().context("loading configuration")?;
    let migrations_dir = std::env::current_dir()
        .context("getting current directory")?
        .join("warren/migrations_atlas");
    let dir_url = format!("file://{}", migrations_dir.display());
    log::info!(
        "running atlas migrate apply against {}",
        redact_url(&cfg.database_url)
    );
    let status = std::process::Command::new("atlas")
        .args([
            "migrate",
            "apply",
            "--url",
            &cfg.database_url,
            "--dir",
            &dir_url,
        ])
        .status()
        .context("spawning atlas (install from https://atlasgo.io)")?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "atlas migrate apply exited with {}",
            status.code().unwrap_or(-1)
        ));
    }
    log_status(&cfg.database_url, &dir_url).await;
    Ok(())
}

async fn log_status(database_url: &str, dir_url: &str) {
    let output = match std::process::Command::new("atlas")
        .args(["migrate", "status", "--url", database_url, "--dir", dir_url])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            log::warn!("could not run `atlas migrate status` for logging: {e}");
            return;
        }
    };
    if !output.status.success() {
        log::warn!(
            "`atlas migrate status` exited with {} (stderr: {})",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return;
    }
    log::info!(
        "applied migrations:\n{}",
        String::from_utf8_lossy(&output.stdout).trim_end()
    );
}

async fn run_dump_schema() -> anyhow::Result<()> {
    use entity::{admin_session, agent, agent_event, channel, request};
    use sea_orm::sea_query::PostgresQueryBuilder;
    use sea_orm::{DatabaseBackend, Schema};

    let schema = Schema::new(DatabaseBackend::Postgres);
    let tables = [
        schema.create_table_from_entity(agent::Entity),
        schema.create_table_from_entity(channel::Entity),
        schema.create_table_from_entity(request::Entity),
        schema.create_table_from_entity(admin_session::Entity),
        schema.create_table_from_entity(agent_event::Entity),
    ];
    for table in tables {
        println!(
            "{};",
            table.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in schema.create_index_from_entity(agent::Entity) {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in schema.create_index_from_entity(channel::Entity) {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in schema.create_index_from_entity(request::Entity) {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in schema.create_index_from_entity(admin_session::Entity) {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in agent_event::extra_indexes() {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in agent::extra_indexes() {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in channel::extra_indexes() {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in request::extra_indexes() {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    for stmt in admin_session::extra_indexes() {
        println!(
            "{};",
            stmt.to_string(PostgresQueryBuilder).trim_end_matches(';')
        );
    }
    Ok(())
}

fn build_router(state: AppState) -> Router {
    let security_headers = tower::ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ));

    Router::new()
        .merge(routes::ui::router())
        .merge(routes::api::router())
        .merge(routes::openapi::router())
        .merge(routes::docs::router(state.clone()))
        .route("/healthz", get(|| async { "ok" }))
        // The four rabbit-lib WebSocket endpoints (`/ws/rabbit`,
        // `/agent/:id/claude/ws`, `/agent/:id/shell/ws`) and the JSON
        // HTTP API (`/api/agents/:id/claude/...`) are all mounted by
        // `rabbit_lib_axum::router` in one merge. The lib is
        // framework-agnostic now — the axum adapter crate does the
        // wiring. We finalize the lib's router with `Arc<ServerState>`
        // (its declared state type), then merge into the outer
        // `Router<AppState>` via the `FromRef` impl above.
        .merge(rabbit_lib_axum::router(state.live.clone()).with_state(state.live.clone()))
        .nest("/static", routes::static_files::router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            refresh_cookie_middleware,
        ))
        .layer(security_headers)
        .with_state(state)
}

/// Sliding-window admin session refresh. The middleware does its own
/// validity+threshold check (one indexed SELECT) and, when the cookie
/// is valid and `<50% * ttl_hours` remain, bumps `expires_at` and
/// attaches a fresh `Set-Cookie` to the response so the browser-side
/// `Max-Age` rolls forward in lockstep with the DB row.
///
/// The auth handler's `AuthContext` extractor runs its own check too
/// (the extractor is what gates the route), so the SQL hit happens
/// twice per cookie-bearing request. Both queries hit
/// `admin_sessions_expires_idx`; cost is one round-trip + one row
/// read. A future optimization is to share the validation result via
/// request extensions and have the extractor short-circuit — see
/// `validate_admin_session_valid_only` for the lower-effort variant.
async fn refresh_cookie_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Capture the token value before `next.run(req)` consumes the
    // request — we need it to rebuild the cookie with the same value
    // but a fresh `Max-Age`.
    let cookie_token = req
        .headers()
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(|s| s.trim())
        .find_map(|kv| {
            kv.strip_prefix(&format!("{COOKIE_NAME}="))
                .map(|v| v.to_string())
        });

    let mut resp = next.run(req).await;

    if let Some(token) = cookie_token {
        // Mirror `validate_admin_session`'s threshold rule (1
        // remaining < 0.5 * ttl_hours). On a refresh path, also bump
        // the DB row — keeps server-side and client-side TTLs in
        // lockstep. The handler's extractor will re-validate and may
        // do its own bump; both writes set `expires_at = now + TTL`,
        // so the double-update is idempotent.
        match auth::validate_admin_session(&state.db, &token, state.config.session_ttl_hours).await
        {
            Ok(auth::SessionOutcome::Valid { refresh: true }) => {
                let value = format!(
                    "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
                    state.config.session_ttl_hours * 3600
                );
                if let Ok(hv) = HeaderValue::from_str(&value) {
                    resp.headers_mut()
                        .insert(HeaderName::from_static("set-cookie"), hv);
                }
            }
            Ok(_) => {}  // valid but not below threshold — no refresh needed
            Err(_) => {} // expired or DB hiccup; let the handler decide
        }
    }
    resp
}

const COOKIE_NAME: &str = auth::SESSION_COOKIE;
