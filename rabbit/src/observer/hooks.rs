use crate::observer::state::State;
use crate::wire::UsageSnapshot;
use anyhow::Result;
use axum::{extract::Path, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObserverEvent {
    pub kind: &'static str,
    #[serde(serialize_with = "serialize_state_opt")]
    pub state: Option<State>,
    pub session_id: Option<String>,
    pub prompt_id: Option<uuid::Uuid>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub usage: Option<UsageSnapshot>,
    pub error: Option<String>,
    pub raw: Option<serde_json::Value>,
}

fn serialize_state_opt<S: serde::Serializer>(s: &Option<State>, ser: S) -> Result<S::Ok, S::Error> {
    match s {
        Some(st) => ser.serialize_some(st.as_str()),
        None => ser.serialize_none(),
    }
}

fn normalize_kind(kind: &str) -> String {
    let mut out = String::with_capacity(kind.len() + 4);
    for (i, ch) in kind.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

#[derive(Clone)]
pub struct ObserverHandle {
    pub tx: broadcast::Sender<ObserverEvent>,
    pub latest_session: Arc<parking_lot::Mutex<Option<String>>>,
}

impl ObserverHandle {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            latest_session: Arc::new(parking_lot::Mutex::new(None)),
        }
    }
}

pub fn router(handle: ObserverHandle) -> Router {
    Router::new()
        .route("/hook/:kind", post(handle_hook))
        .with_state(handle)
}

async fn handle_hook(
    axum::extract::State(handle): axum::extract::State<ObserverHandle>,
    Path(kind): Path<String>,
    Json(ev): Json<HookEvent>,
) -> impl IntoResponse {
    let parsed = parse(&kind, &ev.payload, &handle);
    let _ = handle.tx.send(parsed);
    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

fn parse(kind: &str, payload: &serde_json::Value, handle: &ObserverHandle) -> ObserverEvent {
    let kind_norm = normalize_kind(kind);
    match kind_norm.as_str() {
        "session_start" => {
            let session_id = payload
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            if let Some(s) = &session_id {
                *handle.latest_session.lock() = Some(s.clone());
            }
            ObserverEvent {
                kind: "session",
                state: None,
                session_id,
                prompt_id: None,
                started_at: None,
                ended_at: None,
                usage: None,
                error: None,
                raw: Some(payload.clone()),
            }
        }
        "session_end" => ObserverEvent {
            kind: "session_end",
            state: Some(State::Ended),
            session_id: payload
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            prompt_id: None,
            started_at: None,
            ended_at: Some(chrono::Utc::now()),
            usage: None,
            error: None,
            raw: Some(payload.clone()),
        },
        "user_prompt_submit" => ObserverEvent {
            kind: "prompt_echo",
            state: Some(State::Running),
            session_id: None,
            prompt_id: None,
            started_at: Some(chrono::Utc::now()),
            ended_at: None,
            usage: None,
            error: None,
            raw: Some(payload.clone()),
        },
        "stop" => {
            let usage = payload
                .get("usage")
                .and_then(|v| serde_json::from_value::<UsageSnapshot>(v.clone()).ok());
            ObserverEvent {
                kind: "stop_hook",
                state: Some(State::Idle),
                session_id: None,
                prompt_id: payload
                    .get("prompt_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| uuid::Uuid::parse_str(s).ok()),
                started_at: None,
                ended_at: Some(chrono::Utc::now()),
                usage,
                error: None,
                raw: Some(payload.clone()),
            }
        }
        "notification" => ObserverEvent {
            kind: "log",
            state: None,
            session_id: None,
            prompt_id: None,
            started_at: None,
            ended_at: None,
            usage: None,
            error: None,
            raw: Some(payload.clone()),
        },
        _ => ObserverEvent {
            kind: "log",
            state: None,
            session_id: None,
            prompt_id: None,
            started_at: None,
            ended_at: None,
            usage: None,
            error: None,
            raw: Some(payload.clone()),
        },
    }
}

pub async fn serve(port: u16, handle: ObserverHandle) -> Result<()> {
    let app = router(handle);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    log::info!("observer listening on http://{addr}/hook/:kind");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
