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

use axum::{
    http::{HeaderName, HeaderValue},
    routing::get,
    Router,
};
use clap::{Parser, Subcommand};
use config::Config;
use db::Db;
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
    /// Emit CREATE TABLE SQL for every entity to stdout.
    DumpSchema,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Server => run_server().await,
        Command::DumpSchema => run_dump_schema().await,
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
