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
/// `rabbit-lib-axum` adapter calls this after wrapping an axum `WebSocket`
/// as a `DynWsTransport`; FFI / hyper / tokio-tungstenite
/// embedders can call it directly.
///
/// `tui_size` is the static grid size advertised to the rabbit after
/// the hello. `None` falls back to (120, 40) — the same default warren
/// uses when `TUI_WIDTH` / `TUI_HEIGHT` env vars are unset.
pub async fn handle_session(
    store: Arc<dyn SessionStore>,
    registry: AgentRegistry,
    transport: impl WsTransport + 'static,
    agent_id: uuid::Uuid,
    tui_size: Option<(u16, u16)>,
) -> anyhow::Result<()> {
    let initial = registry.register(agent_id);
    log::info!("rabbit ws connected: agent={}", agent_id);

    let (handle_for_actor, cmd_tx, cmd_rx) = initial.split_for_actor();

    if let Some(mut entry) = registry.get_mut(&agent_id) {
        entry.value_mut().install_cmd_tx(cmd_tx.clone());
    }

    let (tui_cols, tui_rows) = tui_size.unwrap_or((120, 40));
    actor::run(
        store,
        handle_for_actor,
        agent_id,
        transport,
        cmd_rx,
        tui_cols,
        tui_rows,
    )
    .await
}