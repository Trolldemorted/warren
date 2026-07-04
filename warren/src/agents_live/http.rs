use crate::agents_live::handle::AgentStateSnapshot;
use crate::agents_live::wire::{AgentState, UsageSnapshot};
use crate::auth::AuthContext;
use crate::error::{AppError, AppResult};
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json, Router,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
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

async fn require_handle(
    state: &AppState,
    agent_id: Uuid,
) -> AppResult<crate::agents_live::AgentHandle> {
    let handle = state
        .live
        .get(&agent_id)
        .ok_or_else(|| AppError::NotFound)?;
    Ok(handle.clone())
}

#[derive(Deserialize)]
struct PromptReq {
    text: String,
    #[serde(default)]
    wait: bool,
}

#[derive(serde::Serialize)]
struct PromptRes {
    prompt_id: Uuid,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: chrono::DateTime<chrono::Utc>,
}

async fn claude_prompt(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(req): Json<PromptReq>,
) -> AppResult<impl IntoResponse> {
    ctx.require_admin()?;
    let handle = require_handle(&state, id).await?;
    if req.text.trim().is_empty() {
        return Err(AppError::BadRequest("text required".into()));
    }
    if matches!(handle.snapshot().state, AgentState::Running) {
        return Err(AppError::Conflict("agent busy".into()));
    }
    let outcome = handle.prompt(&req.text, req.wait).await?;
    Ok(Json(PromptRes {
        prompt_id: outcome.prompt_id,
        started_at: outcome.started_at,
        ended_at: outcome.ended_at,
    }))
}

#[derive(serde::Serialize)]
struct UsageRes {
    #[serde(flatten)]
    usage: UsageSnapshot,
}

async fn claude_usage(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<UsageRes>> {
    ctx.require_admin()?;
    let handle = require_handle(&state, id).await?;
    let usage = handle.usage().await?;
    Ok(Json(UsageRes { usage }))
}

#[derive(serde::Serialize)]
struct StateRes {
    state: AgentState,
    session_id: Option<String>,
    claude_version: Option<String>,
    connected: bool,
}

async fn claude_state(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<StateRes>> {
    ctx.require_admin()?;
    let connected = state.live.contains_key(&id);
    let snap: AgentStateSnapshot = if connected {
        require_handle(&state, id).await?.state().await?
    } else {
        AgentStateSnapshot::default()
    };
    Ok(Json(StateRes {
        state: snap.state,
        session_id: snap.session_id,
        claude_version: snap.claude_version,
        connected,
    }))
}

#[derive(Deserialize)]
struct ClearReq {
    #[serde(default)]
    hard: bool,
}

async fn claude_clear(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(req): Json<ClearReq>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    let handle = require_handle(&state, id).await?;
    handle.clear(req.hard).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claude_compact(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    let handle = require_handle(&state, id).await?;
    handle.compact().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claude_interrupt(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    let handle = require_handle(&state, id).await?;
    handle.interrupt().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RestartReq {
    #[serde(default)]
    fresh: bool,
}

async fn claude_restart(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(req): Json<RestartReq>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    let handle = require_handle(&state, id).await?;
    handle.restart(req.fresh).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since: i64,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(serde::Serialize)]
struct EventRow {
    id: Uuid,
    agent_id: Uuid,
    seq: i64,
    ts: chrono::DateTime<chrono::Utc>,
    kind: String,
    payload: serde_json::Value,
}

async fn claude_events(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<Vec<EventRow>>> {
    ctx.require_admin()?;
    let limit = q.limit.unwrap_or(500).clamp(1, 5000) as u64;
    let rows = crate::db_ops::list_events_since(&state.db, id, q.since, limit).await?;
    Ok(Json(
        rows.into_iter()
            .map(|m| EventRow {
                id: m.id,
                agent_id: m.agent_id,
                seq: m.seq,
                ts: m.ts,
                kind: m.kind,
                payload: m.payload,
            })
            .collect(),
    ))
}

async fn claude_events_stream(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<impl IntoResponse> {
    ctx.require_admin()?;
    if !state.live.contains_key(&id) {
        return Err(AppError::NotFound);
    }
    let meta_rx = state
        .live
        .get(&id)
        .ok_or(AppError::NotFound)?
        .subscribe_meta();
    let stream = async_stream_stream(meta_rx);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn async_stream_stream(
    mut meta_rx: tokio::sync::broadcast::Receiver<crate::agents_live::wire::EnvelopeBody>,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> {
    Box::pin(async_stream::stream! {
        loop {
            match tokio::time::timeout(Duration::from_secs(15), meta_rx.recv()).await {
                Ok(Ok(ev)) => {
                    let json = serde_json::to_string(&ev).unwrap_or_default();
                    yield Ok::<Event, Infallible>(Event::default().data(json));
                }
                Ok(Err(_)) => {
                    yield Ok::<Event, Infallible>(Event::default().comment("closed"));
                    break;
                }
                Err(_) => {
                    yield Ok::<Event, Infallible>(Event::default().comment("keepalive"));
                }
            }
        }
    })
}
