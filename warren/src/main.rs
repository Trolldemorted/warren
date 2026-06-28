mod auth;
mod config;
mod db;
mod db_ops;
mod entity;
mod error;
mod ids;
mod models;
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
use tower_http::set_header::SetResponseHeaderLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Config,
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
    #[command(name = "applyMigration")]
    ApplyMigration,
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
            Command::ApplyMigration => run_apply_migration().await,
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
        db,
        config: cfg.clone(),
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

async fn run_apply_migration() -> anyhow::Result<()> {
    let cfg = Config::from_env().context("loading configuration")?;
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
    Ok(())
}

async fn run_dump_schema() -> anyhow::Result<()> {
    use entity::{admin_session, agent, request};
    use sea_orm::sea_query::PostgresQueryBuilder;
    use sea_orm::{DatabaseBackend, Schema};

    let schema = Schema::new(DatabaseBackend::Postgres);
    let tables = [
        schema.create_table_from_entity(agent::Entity),
        schema.create_table_from_entity(request::Entity),
        schema.create_table_from_entity(admin_session::Entity),
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
    for stmt in agent::extra_indexes() {
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
        .nest("/static", routes::static_files::router(state.clone()))
        .layer(security_headers)
        .with_state(state)
}
