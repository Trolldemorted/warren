//! §D Milestone 5 — `/agent/:id/shell` WS handler.
//!
//! Counterpart to `ws_browser.rs` but for the shell channel. Pure
//! byte-pump: forwards bytes from rabbit's shell PTY to the browser, and
//! typed bytes from the browser back to rabbit. No meta events, no
//! `Prompt`/`Interrupt`/etc. — those belong on the Claude channel and
//! are gated by the reject-when-Running policy there.
//!
//! The same wire byte (TER CHAN_SHELL = 0x02) is used to route frames to
//! the right PTY on the rabbit side; this handler filters on that byte.

use crate::agents_live::AgentRegistry;
use crate::auth;
use crate::error::AppError;
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use uuid::Uuid;

/// Same `?viewer=true` query contract as `ws_browser`. The shell endpoint
/// is intrinsically a debug surface, but a viewer toggle is still useful
/// for "just watch the shell, don't type" sessions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WsShellQuery {
    #[serde(default)]
    pub viewer: Option<bool>,
}

pub async fn ws_shell(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(q): Query<WsShellQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    if !auth::validate_admin_session(
        &state.db,
        &auth::read_session_cookie(&headers).unwrap_or_default(),
    )
    .await
    .unwrap_or(false)
    {
        return Err(AppError::Unauthorized);
    }
    let viewer_mode = q.viewer.unwrap_or(false);
    let registry: AgentRegistry = state.live.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle(socket, registry, id, viewer_mode).await {
            log::debug!("shell ws closed for agent {}: {e:?}", id);
        }
    }))
}

async fn handle(
    socket: WebSocket,
    registry: AgentRegistry,
    agent_id: Uuid,
    viewer_mode: bool,
) -> anyhow::Result<()> {
    let handle = registry
        .get(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("agent not connected"))?
        .clone();
    let mut term_rx = handle.subscribe_term();

    let (mut sink, mut stream) = socket.split();

    // Replay any buffered shell frames so a late joiner sees the recent
    // shell history (mirrors `ws_browser`'s replay buffer pattern, just
    // filtered to TERM_CHAN_SHELL).
    for chunk in handle.replay_term() {
        if chunk.first() != Some(&crate::agents_live::wire::TERM_CHAN_SHELL) {
            continue;
        }
        if sink.send(Message::Binary(chunk.to_vec())).await.is_err() {
            break;
        }
    }

    loop {
        tokio::select! {
            biased;
            chunk = term_rx.recv() => {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(_) => break,
                };
                // Forward only the shell channel; the claude channel has
                // its own WS endpoint.
                if bytes.first() != Some(&crate::agents_live::wire::TERM_CHAN_SHELL) {
                    continue;
                }
                if sink.send(Message::Binary(bytes.to_vec())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => {
                let Some(msg) = msg else { break; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    Message::Binary(mut b) => {
                        if b.is_empty() { continue; }
                        // §D read-only viewer: drop typed bytes for viewer
                        // connections, mirroring ws_browser's policy.
                        if viewer_mode { continue; }
                        let chan = b.remove(0);
                        if chan != crate::agents_live::wire::TERM_CHAN_SHELL {
                            // Wrong channel — ignore; the client should
                            // only send bytes tagged TERM_CHAN_SHELL on
                            // this WS.
                            continue;
                        }
                        if let Err(e) =
                            handle.send_terminal_bytes(
                                crate::agents_live::wire::TERM_CHAN_SHELL,
                                Bytes::from(b),
                            ).await
                        {
                            log::debug!("shell send_terminal_bytes failed: {e:?}");
                        }
                    }
                    Message::Text(_) | Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
        }
    }
    Ok(())
}