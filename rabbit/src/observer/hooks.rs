use crate::observer::state::State;
use anyhow::Result;
use axum::{extract::Path, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use rabbit_lib::wire::UsageSnapshot;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    /// Latest lifecycle state observed from hook events (§D prompt policy).
    /// The supervisor consults this to decide whether an inbound prompt is
    /// allowed: prompts arriving while `Running` are rejected rather than
    /// injected mid-turn. Transitions to `Running` on `UserPromptSubmit` and
    /// back to `Idle` on `Stop`. Starts `Starting` until the first hook fires.
    latest_state: Arc<parking_lot::Mutex<State>>,
    /// Path to the on-disk transcript file, reported by the `SessionStart`
    /// hook payload as `transcript_path` (§A.3). `None` until the hook fires.
    /// The transcript tailer consults this so it follows the *real* path —
    /// `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` — instead of
    /// guessing.
    latest_transcript_path: Arc<parking_lot::Mutex<Option<PathBuf>>>,
}

impl Default for ObserverHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ObserverHandle {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            tx,
            latest_session: Arc::new(parking_lot::Mutex::new(None)),
            latest_state: Arc::new(parking_lot::Mutex::new(State::Starting)),
            latest_transcript_path: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub fn latest_session(&self) -> Option<String> {
        self.latest_session.lock().clone()
    }

    /// Current lifecycle state as last reported by a hook event.
    pub fn latest_state(&self) -> State {
        *self.latest_state.lock()
    }

    /// Directly set the lifecycle state from the supervisor's *own* transitions
    /// (spawn→`idle`, clean exit→`ended`, crash/shutdown→`dead`). Hook-derived
    /// states (`running`/`idle`) flow through [`ObserverHandle::ingest`]; these
    /// supervisor-side transitions flow through here, so `latest_state()`
    /// reflects the full lifecycle rather than only the hook subset.
    pub fn set_state(&self, state: State) {
        *self.latest_state.lock() = state;
    }

    pub fn latest_transcript_path(&self) -> Option<PathBuf> {
        self.latest_transcript_path.lock().clone()
    }

    /// Parse a hook event and fold it into the handle's tracked state.
    /// Returns the `ObserverEvent` for broadcast. Kept separate from the HTTP
    /// handler so the state-tracking semantics are unit-testable without axum.
    pub fn ingest(&self, kind: &str, payload: &serde_json::Value) -> ObserverEvent {
        let parsed = parse(kind, payload, self);
        // Track the latest lifecycle state so the supervisor can gate inbound
        // prompts (reject-when-Running policy, §D). Only events that carry a
        // concrete state advance it; log/notification events leave it unchanged.
        if let Some(st) = parsed.state {
            *self.latest_state.lock() = st;
        }
        parsed
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
    let parsed = handle.ingest(&kind, &ev.payload);
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
            // §A.3: the SessionStart payload carries the absolute path to the
            // transcript jsonl file (`~/.claude/projects/<encoded-cwd>/<id>.jsonl`).
            // Capture it so the transcript tailer can follow the real file
            // instead of guessing. Some hook implementations nest the field
            // under `payload.transcript_path`; others put it at top level.
            // Probe both.
            let transcript_path = payload
                .get("transcript_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .or_else(|| {
                    payload
                        .get("payload")
                        .and_then(|p| p.get("transcript_path"))
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from)
                });
            if let Some(p) = transcript_path {
                *handle.latest_transcript_path.lock() = Some(p);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn latest_state_starts_starting() {
        let h = ObserverHandle::new();
        assert_eq!(h.latest_state(), State::Starting);
    }

    #[test]
    fn user_prompt_submit_marks_running_then_stop_marks_idle() {
        let h = ObserverHandle::new();
        h.ingest("UserPromptSubmit", &json!({}));
        assert_eq!(h.latest_state(), State::Running);
        h.ingest("Stop", &json!({}));
        assert_eq!(h.latest_state(), State::Idle);
    }

    #[test]
    fn notification_does_not_clobber_running_state() {
        // Log/notification events carry no state; they must leave the tracked
        // state intact so the reject-when-Running gate stays correct mid-turn.
        let h = ObserverHandle::new();
        h.ingest("UserPromptSubmit", &json!({}));
        h.ingest("Notification", &json!({"message": "waiting"}));
        assert_eq!(h.latest_state(), State::Running);
    }

    #[test]
    fn session_end_marks_ended() {
        let h = ObserverHandle::new();
        h.ingest("SessionEnd", &json!({"session_id": "abc"}));
        assert_eq!(h.latest_state(), State::Ended);
    }
}
