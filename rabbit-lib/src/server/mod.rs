//! Server half of the rabbit protocol — the multi-broker WebSocket
//! runtime that fans a single supervisor's term-bytes and meta-envelopes
//! out to many browser subscribers per agent, persists the event stream
//! via the [`SessionStore`] trait, and authenticates connections via
//! the [`AuthBackend`] trait.
//!
//! Modules:
//! - [`handle`] — the per-agent live handle (idle, prompt, interrupt,
//!   clear, compact, restart, resize, snapshot, terminal replay buffer)
//! - [`actor`] — the supervisor-side task that owns the rabbit WS
//!   connection and runs the per-agent prompt loop
//! - [`registry`] — the `DashMap<Uuid, AgentHandle>` shared between
//!   the rabbit WS, the browser WS, the shell WS, and the HTTP API
//! - [`ws_rabbit`] — `/ws/rabbit` (inbound supervisor connection)
//! - [`ws_browser`] — `/agent/:id/claude/ws` (browser subscriber)
//! - [`ws_shell`]  — `/agent/:id/shell/ws` (browser ↔ bash PTY)
//! - [`http`]      — `/api/agents/:id/claude/{prompt,usage,state,...}`
//!
//! Embedders construct a [`ServerState`] with concrete implementations
//! of [`SessionStore`] / [`AuthBackend`] / [`LogSink`] and call
//! [`ServerState::router`] to obtain an `axum::Router<Arc<ServerState>>`
//! that they can merge into their own larger router (see
//! `rabbit-lib/README.md` for the embedding recipe).
//!
//! See `rabbit-lib.md` §3 for the full design rationale.

use std::sync::Arc;
use uuid::Uuid;

use self::registry::AgentRegistry;

// ----- Records returned by the storage trait -----

/// A single event row returned by [`SessionStore::list_events_since`].
/// Field shape mirrors the on-disk agent_event row: the embedder's
/// `SeaOrmSessionStore` populates it from a SeaORM model; the rabbit-lib
/// HTTP layer serializes it as JSON to the API client.
#[derive(Debug, Clone)]
pub struct AgentEventRecord {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub seq: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub payload: serde_json::Value,
}

// ----- Traits external embedders must implement -----

/// Persistent log of `(agent_id, seq)` events that the actor writes as
/// it processes a supervisor's link frames. The trait is intentionally
/// tiny (three methods) so external storage backends are easy to
/// provide; the on-disk SeaORM rows are one concrete implementation.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Returns the next free `seq` for `agent_id`'s event log. The
    /// store is responsible for persisting this on insert; the caller
    /// uses it as the dedup watermark and to populate
    /// `(agent_id, seq)` rows.
    async fn next_event_seq(&self, agent_id: Uuid) -> anyhow::Result<i64>;

    /// Append one event to `agent_id`'s log at `seq`. Returns
    /// `Ok(())` on success, `Err(_)` on transport failure.
    /// Implementations may surface a unique-constraint violation
    /// (re-insert at an existing seq) however they like; the actor
    /// swallows it as "already persisted" via `.ok()`.
    async fn insert_event(
        &self,
        agent_id: Uuid,
        seq: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<()>;

    /// Return up to `limit` events for `agent_id` with `seq > since`,
    /// ordered ascending by `seq`. Used by the HTTP
    /// `claude_events` endpoint to back the event-log UI.
    async fn list_events_since(
        &self,
        agent_id: Uuid,
        since: i64,
        limit: u64,
    ) -> anyhow::Result<Vec<AgentEventRecord>>;
}

/// Auth surface for inbound connections: rabbit (agent-token) and
/// browser (admin-session-cookie). The trait returns the authenticated
/// identity on success or a structured [`AuthError`] on failure.
#[async_trait::async_trait]
pub trait AuthBackend: Send + Sync + 'static {
    /// Validate the `Authorization: Bearer …` header against the
    /// agent-token table. Returns the authenticated `agent_id` on
    /// success.
    async fn authenticate_agent(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<Uuid, AuthError>;

    /// Validate the session cookie / header for admin endpoints.
    /// Returns `true` iff the caller is an authenticated admin.
    async fn authenticate_admin(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<bool, AuthError>;
}

/// Reasons the auth trait can reject a request. War maps this onto its
/// own `AppError::Unauthorized` via a `From` impl in the embedder
/// adapter.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or malformed authorization header")]
    Missing,
    #[error("invalid credentials")]
    Invalid,
    #[error("internal auth failure: {0}")]
    Internal(String),
}

/// axum-compatible error type returned by the lib's HTTP handlers.
/// Wraps [`AuthError`] (for the auth gate) and `anyhow::Error` (for
/// anything else). `IntoResponse` maps each variant to the right
/// status code so the embedder doesn't have to write a per-handler
/// conversion.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl axum::response::IntoResponse for ServerError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        use axum::Json;
        match self {
            ServerError::Auth(AuthError::Missing) => (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "missing or malformed credentials"})),
            )
                .into_response(),
            ServerError::Auth(AuthError::Invalid) => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "invalid credentials"})),
            )
                .into_response(),
            ServerError::Auth(AuthError::Internal(e)) => {
                log::error!("auth internal error: {e:?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal auth failure"})),
                )
                    .into_response()
            }
            ServerError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not found"})),
            )
                .into_response(),
            ServerError::Conflict(msg) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response(),
            ServerError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response(),
            ServerError::Other(e) => {
                log::error!("server handler error: {e:?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal error"})),
                )
                    .into_response()
            }
        }
    }
}

/// Convenience alias for the lib's HTTP handler return type.
pub(crate) type ServerResult<T> = std::result::Result<T, ServerError>;

/// Surface for the actor's `log` calls. Default impl is `log::log!`.
/// Embedders can swap in a structured logger.
pub trait LogSink: Send + Sync + 'static {
    fn log(&self, level: log::Level, target: &str, message: &str);
}

/// Default `LogSink` that just forwards to the `log` crate's macros.
pub struct StdLogSink;

impl LogSink for StdLogSink {
    fn log(&self, level: log::Level, target: &str, message: &str) {
        log::log!(target: target, level, "{}", message);
    }
}

// ----- ServerState (the lib's analogue of warren's AppState) -----

/// The state type the lib's axum handlers take. Embedders construct it
/// once and pass it to [`ServerState::router`] (or merge the router
/// into their own `axum::Router`).
pub struct ServerState {
    pub registry: AgentRegistry,
    pub store: Arc<dyn SessionStore>,
    pub auth: Arc<dyn AuthBackend>,
    pub log_sink: Arc<dyn LogSink>,
}

impl ServerState {
    /// Build an `axum::Router<Arc<ServerState>>` mounting `/ws/rabbit`,
    /// `/agent/:id/claude/ws`, `/agent/:id/shell/ws`, and the JSON HTTP
    /// endpoints (`/api/agents/:id/claude/...`). The router carries
    /// `Arc<ServerState>` as its state; the embedder must call
    /// `.with_state(state.clone())` (or merge into a larger router that
    /// finalizes with an outer state) before serving.
    pub fn router(self: &Arc<Self>) -> axum::Router<Arc<Self>> {
        axum::Router::new()
            .merge(ws_rabbit::router())
            .merge(ws_browser::router())
            .merge(ws_shell::router())
            .merge(http::router())
    }
}

// Server modules — the implementation that owns the per-agent live
// handles and the WebSocket / HTTP surfaces that front the supervisor
// half of the lib.
//
// `registry` stays pub because embedders construct and reference
// `AgentRegistry` (warren's `/agent/:id/claude/history` route takes
// `&AgentRegistry`). The other five are crate-internal: their public
// surfaces are reached only through `ServerState::router`, which is
// what embedders actually wire up.
pub mod registry;

pub(crate) mod actor;
pub(crate) mod handle;
pub(crate) mod http;
pub(crate) mod ws_browser;
pub(crate) mod ws_rabbit;
pub(crate) mod ws_shell;
