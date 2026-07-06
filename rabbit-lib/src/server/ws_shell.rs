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

use crate::server::registry::AgentRegistry;
use crate::wire::TermFrame;

use crate::server::{AuthError, ServerError, ServerState};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use uuid::Uuid;

/// `axum` router for the shell-side WebSocket. Mounts
/// `/agent/:id/shell/ws`. Carries `Arc<ServerState>` as its state
/// type — the embedder is expected to call `.with_state(...)` on the
/// parent router.
pub fn router() -> axum::Router<Arc<ServerState>> {
    axum::Router::new().route("/agent/:id/shell/ws", axum::routing::get(ws_shell))
}

/// Same `?viewer=true` query contract as `ws_browser`. The shell endpoint
/// is intrinsically a debug surface, but a viewer toggle is still useful
/// for "just watch the shell, don't type" sessions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WsShellQuery {
    #[serde(default)]
    pub viewer: Option<bool>,
}

pub async fn ws_shell(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(q): Query<WsShellQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, ServerError> {
    if !state.auth.authenticate_admin(&headers).await? {
        return Err(ServerError::Auth(AuthError::Invalid));
    }
    let viewer_mode = q.viewer.unwrap_or(false);
    let registry: AgentRegistry = state.registry.clone();
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
    // Split the socket first so the wait-for-arrival guard below can
    // observe early client closes without burning the upgrade. Mirrors
    // the gate in `ws_browser::handle` — see the comment there for the
    // full rationale.
    let (mut sink, mut stream) = socket.split();
    let handle = loop {
        if let Some(h) = registry.get(&agent_id) {
            break h.clone();
        }
        let mut notified = std::pin::pin!(registry.wait_for_arrival());
        tokio::select! {
            _ = notified.as_mut() => continue,
            _msg = stream.next() => {
                return Ok(());
            }
        }
    };
    let mut term_rx = handle.subscribe_term();

    // Replay any buffered shell frames so a late joiner sees the recent
    // shell history (mirrors `ws_browser`'s replay buffer pattern, just
    // filtered to TERM_CHAN_SHELL). §A.7: each frame is re-emitted as
    // `<chan:1> <seq:8 BE> <data>`, preserving the seq the shell reader
    // thread assigned on the rabbit side.
    for TermFrame { chan, seq, data } in handle.replay_term() {
        if chan != crate::wire::TERM_CHAN_SHELL {
            continue;
        }
        if data.is_empty() {
            continue;
        }
        let mut frame = Vec::with_capacity(9 + data.len());
        frame.push(chan);
        frame.extend_from_slice(&seq.to_be_bytes());
        frame.extend_from_slice(&data);
        if sink.send(Message::Binary(frame)).await.is_err() {
            break;
        }
    }

    loop {
        tokio::select! {
            biased;
            chunk = term_rx.recv() => {
                let frame = match chunk {
                    Ok(f) => f,
                    Err(_) => break,
                };
                // §A.7: dumb-pipe pass-through for the shell channel.
                // Re-emit `<chan:1> <seq:8 BE> <data>` so the browser pane
                // can match live shell bytes against any future
                // snapshot's `after_seq`. (Today there's no shell-side VT
                // so no snapshot is ever emitted; the seq still rides
                // through for protocol symmetry.)
                let TermFrame { chan, seq, data } = frame;
                if chan != crate::wire::TERM_CHAN_SHELL {
                    continue;
                }
                if data.is_empty() {
                    continue;
                }
                let mut out = Vec::with_capacity(9 + data.len());
                out.push(chan);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(&data);
                if sink.send(Message::Binary(out)).await.is_err() {
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
                        if chan != crate::wire::TERM_CHAN_SHELL {
                            // Wrong channel — ignore; the client should
                            // only send bytes tagged TERM_CHAN_SHELL on
                            // this WS.
                            continue;
                        }
                        if let Err(e) =
                            handle.send_terminal_bytes(
                                crate::wire::TERM_CHAN_SHELL,
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
