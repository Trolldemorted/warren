//! Axum adapter for `rabbit-lib`.
//!
//! `rabbit-lib`'s `server` module is framework-agnostic: it exposes
//! transport traits, plain async domain functions, and typed error
//! values. Embedders using axum get a thin shim here that wires those
//! pieces onto an `axum::Router`.
//!
//! Public surface:
//! - [`router`] — single entry point. Returns an
//!   `axum::Router<Arc<ServerState>>` carrying the WS upgrade handlers
//!   (`/ws/rabbit`, `/agent/:id/claude/ws`, `/agent/:id/shell/ws`)
//!   plus the 9 HTTP routes from `ServerState::http_*`.
//! - [`ws_transport`] — utility for embedders building their own
//!   upgrade paths: wraps an `axum::extract::ws::WebSocket` as a
//!   `rabbit_lib::server::DynWsTransport`. The router above uses this
//!   internally.
//!
//! Embedders that don't use axum (hyper, actix, tokio-tungstenite
//! directly, FFI bindings) skip this crate entirely and call the lib's
//! domain functions / `WsTransport` impls straight up.

use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use serde::Deserialize;
use uuid::Uuid;

use rabbit_lib::server::registry::AgentRegistry;
use rabbit_lib::server::DynWsTransport;
use rabbit_lib::server::{AuthError, ServerError, ServerState};

use crate::http::AxumServerError;
use crate::ws_bridge::axum_ws_transport;

/// Build the full axum surface for `rabbit-lib`. Returns an
/// unfinalized `Router<Arc<ServerState>>` that the embedder must
/// finalize with `.with_state(state)` (when the outer router also
/// carries `Arc<ServerState>`) or by merging into a larger router that
/// exposes `Arc<ServerState>` via a `FromRef` impl.
///
/// The `state` argument is unused at runtime — it only carries the
/// type information axum needs to compile the handler signatures.
/// Embedders building a single-tenant router call this with their
/// `Arc<ServerState>`; embedders that compose the lib into a larger
/// app (warren) merge the returned sub-router into theirs and rely on
/// the `FromRef` impl on `Arc<ServerState>` to extract the lib state
/// at request time.
pub fn router(state: Arc<ServerState>) -> axum::Router<Arc<ServerState>> {
    let _ = state; // silence unused-argument warnings on the type-only path
    axum::Router::new()
        .route("/ws/rabbit", axum::routing::get(ws_rabbit_handler))
        .route(
            "/agent/:id/claude/ws",
            axum::routing::get(ws_browser_handler),
        )
        .route("/agent/:id/shell/ws", axum::routing::get(ws_shell_handler))
        .merge(http::router())
}

/// Wrap an `axum::extract::ws::WebSocket` as a `DynWsTransport` so the
/// framework-agnostic `WsTransport`-shaped APIs in `rabbit-lib` can
/// drive it. Exposed for embedders that want to compose the lib into a
/// custom upgrade path (e.g. when the upgrade is gated behind a tower
/// middleware or a different matcher).
pub fn ws_transport(socket: WebSocket) -> DynWsTransport {
    axum_ws_transport(socket)
}

// ----- WS upgrade handlers -----

async fn ws_rabbit_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AxumServerError> {
    let agent_id = state
        .auth
        .authenticate_agent(&headers)
        .await
        .map_err(|e| AxumServerError(ServerError::Auth(e)))?;
    Ok(ws.on_upgrade(move |socket| async move {
        let store = state.store.clone();
        let registry = state.registry.clone();
        let transport = axum_ws_transport(socket);
        if let Err(e) =
            rabbit_lib::server::ws_rabbit::handle_session(store, registry, transport, agent_id)
                .await
        {
            log::debug!("rabbit ws closed for agent {}: {e:?}", agent_id);
        }
    }))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WsBrowserQuery {
    #[serde(default)]
    viewer: Option<bool>,
}

async fn ws_browser_handler(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(q): Query<WsBrowserQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AxumServerError> {
    let admin_ok = state
        .auth
        .authenticate_admin(&headers)
        .await
        .map_err(|e| AxumServerError(ServerError::Auth(e)))?;
    if !admin_ok {
        return Err(AxumServerError(ServerError::Auth(AuthError::Invalid)));
    }
    let viewer_mode = q.viewer.unwrap_or(false);
    let registry: AgentRegistry = state.registry.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        let transport = axum_ws_transport(socket);
        if let Err(e) =
            rabbit_lib::server::ws_browser::handle(transport, registry, id, viewer_mode).await
        {
            log::debug!("browser ws closed for agent {}: {e:?}", id);
        }
    }))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WsShellQuery {
    #[serde(default)]
    viewer: Option<bool>,
}

async fn ws_shell_handler(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(q): Query<WsShellQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AxumServerError> {
    let admin_ok = state
        .auth
        .authenticate_admin(&headers)
        .await
        .map_err(|e| AxumServerError(ServerError::Auth(e)))?;
    if !admin_ok {
        return Err(AxumServerError(ServerError::Auth(AuthError::Invalid)));
    }
    let viewer_mode = q.viewer.unwrap_or(false);
    let registry: AgentRegistry = state.registry.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        let transport = axum_ws_transport(socket);
        if let Err(e) =
            rabbit_lib::server::ws_shell::handle(transport, registry, id, viewer_mode).await
        {
            log::debug!("shell ws closed for agent {}: {e:?}", id);
        }
    }))
}

// Re-export the HTTP adapter layer and the axum WS bridge.
pub mod http;
mod ws_bridge;
