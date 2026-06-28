mod auth;
mod config;
mod db;
mod db_ops;
mod entity;
mod error;
mod ids;
mod migration;
mod models;
mod routes;
mod templates;

use axum::{
    http::{HeaderName, HeaderValue},
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use config::Config;
use db::Db;
use sea_orm_migration::MigratorTrait;
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
    /// Apply pending database migrations and exit.
    #[command(name = "applyMigrations")]
    ApplyMigrations,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Server => run_server().await,
        Command::ApplyMigrations => run_apply_migrations().await,
    }
}

async fn run_server() -> anyhow::Result<()> {
    simple_logger::init_with_env()?;

    let cfg = Config::from_env()?;
    let db = db::connect(&cfg.database_url).await?;
    let state = AppState {
        db,
        config: cfg.clone(),
    };
    let app = build_router(state);

    let addr: SocketAddr = cfg.bind_addr.parse()?;
    log::info!("warren listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

async fn run_apply_migrations() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let db = db::connect(&cfg.database_url).await?;
    migration::Migrator::up(&db, None).await?;
    log::info!("migrations applied");
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
