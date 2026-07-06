use crate::server::actor;
use crate::server::registry::AgentRegistry;
use crate::server::ServerResult;
use super::ServerState;
use crate::server::SessionStore;
use crate::wire::{
    AgentState, Envelope, EnvelopeBody, HelloDown, TermSize, PROTOCOL_VERSION,
};
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use std::sync::Arc;

/// `axum` router for the rabbit-side WebSocket. Mounts `/ws/rabbit`.
/// Carries `Arc<ServerState>` as its state type — the embedder is
/// expected to call `.with_state(...)` on the parent router.
pub fn router() -> axum::Router<Arc<ServerState>> {
    axum::Router::new().route("/ws/rabbit", axum::routing::get(ws_rabbit))
}

pub async fn ws_rabbit(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ServerResult<impl IntoResponse> {
    let agent_id = state.auth.authenticate_agent(&headers).await?;

    Ok(ws.on_upgrade(move |socket| async move {
        let store = state.store.clone();
        let registry = state.registry.clone();
        if let Err(e) = handle_session(store, registry, socket, agent_id).await {
            log::debug!("rabbit ws closed for agent {}: {e:?}", agent_id);
        }
    }))
}

async fn handle_session(
    store: Arc<dyn SessionStore>,
    registry: AgentRegistry,
    socket: WebSocket,
    agent_id: uuid::Uuid,
) -> anyhow::Result<()> {
    let initial = registry.register(agent_id);
    log::info!("rabbit ws connected: agent={}", agent_id);

    let (handle_for_actor, cmd_tx, cmd_rx) = initial.split_for_actor();

    if let Some(mut entry) = registry.get_mut(&agent_id) {
        entry.value_mut().install_cmd_tx(cmd_tx.clone());
    }

    actor::run(store, handle_for_actor, agent_id, socket, cmd_rx).await
}

#[allow(dead_code)]
pub fn _placeholder_hello(agent_id: uuid::Uuid) -> Envelope {
    Envelope {
        v: PROTOCOL_VERSION,
        seq: 1,
        body: EnvelopeBody::Hello(HelloDown {
            agent_id,
            protocol_v: PROTOCOL_VERSION,
            claude_version: "unimplemented".to_string(),
            session_id: None,
            state: AgentState::Starting,
            term_size: TermSize {
                cols: 120,
                rows: 40,
            },
            recorder_url: None,
        }),
    }
}
