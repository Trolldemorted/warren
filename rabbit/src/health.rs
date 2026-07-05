use anyhow::Result;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct HealthState {
    pub child_alive: Arc<AtomicBool>,
    pub shutting_down: Arc<AtomicBool>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthState {
    pub fn new() -> Self {
        Self {
            child_alive: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_alive(&self, alive: bool) {
        self.child_alive.store(alive, Ordering::Relaxed);
    }

    pub fn set_shutting_down(&self, value: bool) {
        self.shutting_down.store(value, Ordering::SeqCst);
    }
}

pub async fn serve(port: u16, state: HealthState) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state);
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    log::info!("rabbit health server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn readyz(State(state): State<HealthState>) -> impl IntoResponse {
    if state.shutting_down.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, "shutting down");
    }
    if state.child_alive.load(Ordering::Relaxed) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "child not running")
    }
}
