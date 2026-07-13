//! Thin axum adapter layer for the lib's HTTP API, plus the
//! `ServerError::IntoResponse` impl that was relocated from
//! `rabbit-lib`'s `mod.rs` as part of decoupling the lib from axum.
//!
//! Each handler is a translation between the HTTP shape (status code,
//! JSON body, SSE response) and the framework-agnostic domain methods
//! on [`rabbit_lib::server::http_api::ServerState`]. Domain logic
//! lives in `rabbit-lib::server::http_api`; this module owns the HTTP
//! plumbing only.

use rabbit_lib::server::http_api::{
    ClearRequest, EventRow, EventsQuery, PromptRequest, PromptResponse, RestartRequest,
    StateResponse, UsageResponse,
};
use rabbit_lib::server::{ServerError, ServerState, StoreError};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json, Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Build the HTTP route sub-router for the lib. Returns an
/// unfinalized `Router<Arc<ServerState>>` so the embedder can finalize
/// the state type — typically by `.with_state(state)` if the embedder
/// carries `Arc<ServerState>` directly, or by `.merge(...)` into a
/// larger router that exposes `Arc<ServerState>` via a `FromRef` impl.
pub fn router() -> Router<Arc<ServerState>> {
    Router::new()
        .route(
            "/api/agents/:id/claude/prompt",
            axum::routing::post(claude_prompt),
        )
        .route(
            "/api/agents/:id/claude/usage",
            axum::routing::get(claude_usage),
        )
        .route(
            "/api/agents/:id/claude/state",
            axum::routing::get(claude_state),
        )
        .route(
            "/api/agents/:id/claude/clear",
            axum::routing::post(claude_clear),
        )
        .route(
            "/api/agents/:id/claude/compact",
            axum::routing::post(claude_compact),
        )
        .route(
            "/api/agents/:id/claude/interrupt",
            axum::routing::post(claude_interrupt),
        )
        .route(
            "/api/agents/:id/claude/usage_check",
            axum::routing::post(claude_usage_check),
        )
        .route(
            "/api/agents/:id/claude/context_check",
            axum::routing::post(claude_context_check),
        )
        .route(
            "/api/agents/:id/claude/restart",
            axum::routing::post(claude_restart),
        )
        .route(
            "/api/agents/:id/claude/events",
            axum::routing::get(claude_events),
        )
        .route(
            "/api/agents/:id/claude/events/stream",
            axum::routing::get(claude_events_stream),
        )
}

// ----- Wire types (axum extractor / response shapes) -----

#[derive(Deserialize)]
struct PromptReq {
    text: String,
    #[serde(default)]
    wait: bool,
}

#[derive(Serialize)]
struct PromptRes {
    prompt_id: Uuid,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: chrono::DateTime<chrono::Utc>,
}

impl From<PromptResponse> for PromptRes {
    fn from(r: PromptResponse) -> Self {
        Self {
            prompt_id: r.prompt_id,
            started_at: r.started_at,
            ended_at: r.ended_at,
        }
    }
}

#[derive(Serialize)]
struct UsageRes {
    #[serde(flatten)]
    usage: rabbit_lib::wire::UsageSnapshot,
}

impl From<UsageResponse> for UsageRes {
    fn from(r: UsageResponse) -> Self {
        Self { usage: r.usage }
    }
}

#[derive(Serialize)]
struct StateRes {
    state: rabbit_lib::wire::AgentState,
    session_id: Option<String>,
    claude_version: Option<String>,
    connected: bool,
}

impl From<StateResponse> for StateRes {
    fn from(r: StateResponse) -> Self {
        Self {
            state: r.state,
            session_id: r.session_id,
            claude_version: r.claude_version,
            connected: r.connected,
        }
    }
}

#[derive(Deserialize)]
struct ClearReq {
    #[serde(default)]
    hard: bool,
}

#[derive(Deserialize)]
struct RestartReq {
    #[serde(default)]
    fresh: bool,
}

#[derive(Deserialize)]
struct EventsQueryWire {
    #[serde(default)]
    since: i64,
    #[serde(default)]
    limit: Option<u64>,
}

impl From<EventsQueryWire> for EventsQuery {
    fn from(q: EventsQueryWire) -> Self {
        Self {
            since: q.since,
            limit: q.limit,
        }
    }
}

#[derive(Serialize)]
struct EventRowWire {
    id: Uuid,
    agent_id: Uuid,
    seq: i64,
    ts: chrono::DateTime<chrono::Utc>,
    kind: String,
    payload: serde_json::Value,
}

impl From<EventRow> for EventRowWire {
    fn from(r: EventRow) -> Self {
        Self {
            id: r.id,
            agent_id: r.agent_id,
            seq: r.seq,
            ts: r.ts,
            kind: r.kind,
            payload: r.payload,
        }
    }
}

// ----- Handlers (axum adapters) -----

async fn claude_prompt(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<PromptReq>,
) -> Result<impl IntoResponse, AxumServerError> {
    let resp = state
        .http_prompt(
            &headers,
            id,
            PromptRequest {
                text: req.text,
                wait: req.wait,
            },
        )
        .await?;
    Ok(Json(PromptRes::from(resp)))
}

async fn claude_usage(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<UsageRes>, AxumServerError> {
    let resp = state.http_usage(&headers, id).await?;
    Ok(Json(UsageRes::from(resp)))
}

async fn claude_state(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<StateRes>, AxumServerError> {
    let resp = state.http_state(&headers, id).await?;
    Ok(Json(StateRes::from(resp)))
}

async fn claude_clear(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<ClearReq>,
) -> Result<StatusCode, AxumServerError> {
    state
        .http_clear(&headers, id, ClearRequest { hard: req.hard })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claude_compact(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AxumServerError> {
    state.http_compact(&headers, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claude_interrupt(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AxumServerError> {
    state.http_interrupt(&headers, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claude_usage_check(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AxumServerError> {
    state.http_usage_check(&headers, id).await?;
    // 202 Accepted: the request triggered an async scrape on the
    // rabbit side; the parsed limits arrive on the SSE stream a
    // moment later. The browser already has the open SSE connection
    // and re-renders the Usage panel on the next `case 'usage':`.
    Ok(StatusCode::ACCEPTED)
}

async fn claude_context_check(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AxumServerError> {
    state.http_context_check(&headers, id).await?;
    // §Context-window: 202 Accepted mirrors `claude_usage_check`.
    // The request triggered an async scrape on the rabbit side; the
    // parsed context-window usage arrives on the SSE stream a moment
    // later. The browser already has the open SSE connection and
    // re-renders the Context window panel on the next `case 'usage':`.
    Ok(StatusCode::ACCEPTED)
}

async fn claude_restart(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<RestartReq>,
) -> Result<StatusCode, AxumServerError> {
    state
        .http_restart(&headers, id, RestartRequest { fresh: req.fresh })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claude_events(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQueryWire>,
) -> Result<Json<Vec<EventRowWire>>, AxumServerError> {
    let rows = state.http_events(&headers, id, q.into()).await?;
    Ok(Json(rows.into_iter().map(EventRowWire::from).collect()))
}

async fn claude_events_stream(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AxumServerError> {
    // Use the pre-formatted SSE-bytes shape; the axum adapter just
    // bridges the byte stream into `Sse<Event>`. Embedders using a
    // non-axum framework can call `http_events_stream` directly for
    // envelope items or `http_events_stream_sse` for raw bytes.
    let mut byte_stream = state.http_events_stream_sse(&headers, id).await?;
    let event_stream = async_stream::stream! {
        while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    yield Ok::<Event, Infallible>(Event::default().data(text));
                }
                Err(_e) => break,
            }
        }
    };
    Ok(Sse::new(event_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

// ----- ServerError → IntoResponse -----
//
// The lib's `ServerError` type is foreign to this crate, so we wrap
// it in a local newtype and implement `IntoResponse` for the newtype.
// This is the orphan-rule-compliant way to translate foreign errors
// into axum responses — the adapter's handlers return
// `Result<T>` which `?`-converts into `Result<T, ServerError>`
// and then handlers map that into the newtype.

/// Newtype wrapper around `ServerError` so we can implement
/// `IntoResponse` here (orphan rule). The handler signatures still
/// return `Result<T>`, and they wrap the foreign error into
/// this newtype with `Into` before letting axum serialize it.
pub struct AxumServerError(pub ServerError);

impl From<ServerError> for AxumServerError {
    fn from(e: ServerError) -> Self {
        AxumServerError(e)
    }
}

impl IntoResponse for AxumServerError {
    fn into_response(self) -> axum::response::Response {
        let err = self.0;
        use axum::http::StatusCode;
        use axum::Json;
        match err {
            ServerError::Auth(rabbit_lib::server::AuthError::Missing) => (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "missing or malformed credentials"})),
            )
                .into_response(),
            ServerError::Auth(rabbit_lib::server::AuthError::Invalid) => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "invalid credentials"})),
            )
                .into_response(),
            ServerError::Auth(rabbit_lib::server::AuthError::Internal(e)) => {
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
            ServerError::Store(StoreError::Duplicate) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "already persisted"})),
            )
                .into_response(),
            ServerError::Store(StoreError::Invalid(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response(),
            ServerError::Store(store_err) => {
                // `StoreError` is `#[non_exhaustive]`; cover the
                // remaining variants (Unavailable, Other, plus any
                // future additions) with a single arm that maps them
                // to 503. The detailed `Display` is logged but not
                // surfaced to clients (storage internals are not the
                // embedder's problem).
                log::error!("store error: {store_err}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "storage unavailable"})),
                )
                    .into_response()
            }
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
