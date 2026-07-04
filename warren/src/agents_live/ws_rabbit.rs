use crate::agents_live::actor;
use crate::agents_live::wire::{
    AgentState, Envelope, EnvelopeBody, HelloDown, TermSize, PROTOCOL_VERSION,
};
use crate::agents_live::AgentRegistry;
use crate::auth::extract_agent_token;
use crate::error::AppError;
use crate::AppState;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;

pub async fn ws_rabbit(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    let agent = match extract_agent_token(&state.db, &headers).await? {
        Some(a) => a,
        None => return Err(AppError::Unauthorized),
    };

    Ok(ws.on_upgrade(move |socket| async move {
        let db = state.db.clone();
        let registry = state.live.clone();
        if let Err(e) = handle_session(db, registry, socket, agent.0.id).await {
            log::debug!("rabbit ws closed for agent {}: {e:?}", agent.0.id);
        }
    }))
}

async fn handle_session(
    db: crate::db::Db,
    registry: AgentRegistry,
    socket: WebSocket,
    agent_id: uuid::Uuid,
) -> anyhow::Result<()> {
    use crate::agents_live::handle::AgentHandle;

    let initial = registry
        .entry(agent_id)
        .or_insert_with(|| AgentHandle::new(agent_id))
        .clone();
    log::info!("rabbit ws connected: agent={}", agent_id);

    let (handle_for_actor, cmd_tx, cmd_rx) = initial.split_for_actor();

    if let Some(mut entry) = registry.get_mut(&agent_id) {
        entry.value_mut().install_cmd_tx(cmd_tx.clone());
    }

    actor::run(db, handle_for_actor, agent_id, socket, cmd_rx).await
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
        }),
    }
}
