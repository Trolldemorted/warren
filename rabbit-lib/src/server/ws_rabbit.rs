//! Framework-agnostic rabbit-side session loop.
//!
//! Pure WS framing / supervisor logic — the actor-driven loop that
//! drives the `WsTransport` and exposes `split_for_actor` to the
//! caller. The `rabbit-lib-axum` adapter crate wraps an axum `WebSocket`
//! as a `DynWsTransport` and calls `handle_session` here.

use crate::server::actor;
use crate::server::registry::AgentRegistry;
use crate::server::SessionStore;
use crate::server::WsTransport;
use std::sync::Arc;

/// Framework-agnostic supervisor-side session loop. The
/// `rabbit-lib-axum` adapter calls this after wrapping an axum
/// `WebSocket` as a `DynWsTransport`; FFI / hyper / tokio-tungstenite
/// embedders can call it directly.
pub async fn handle_session(
    store: Arc<dyn SessionStore>,
    registry: AgentRegistry,
    transport: impl WsTransport + 'static,
    agent_id: uuid::Uuid,
) -> anyhow::Result<()> {
    let initial = registry.register(agent_id);
    log::info!("rabbit ws connected: agent={}", agent_id);

    let (handle_for_actor, cmd_tx, cmd_rx) = initial.split_for_actor();

    if let Some(mut entry) = registry.get_mut(&agent_id) {
        entry.value_mut().install_cmd_tx(cmd_tx.clone());
    }

    actor::run(store, handle_for_actor, agent_id, transport, cmd_rx).await
}
