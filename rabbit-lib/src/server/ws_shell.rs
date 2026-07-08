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
use crate::server::transport::TransportMsg;
use crate::server::WsTransport;
use crate::wire::TermFrame;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use uuid::Uuid;

/// Framework-agnostic shell-side session loop. Public so the
/// `rabbit-lib-axum` adapter can call it after wrapping an axum
/// `WebSocket` as a `DynWsTransport`. Kept in `rabbit-lib` because the
/// logic (term-pump, viewer drop) is framework-free.
pub async fn handle(
    transport: impl WsTransport,
    registry: AgentRegistry,
    agent_id: Uuid,
    viewer_mode: bool,
) -> anyhow::Result<()> {
    // Split the transport first so the wait-for-arrival guard below can
    // observe early client closes without burning the upgrade. Mirrors
    // the gate in `ws_browser::handle` — see the comment there for the
    // full rationale.
    let (mut sink, mut stream) = transport.split();
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
        if sink.send(TransportMsg::Binary(frame)).await.is_err() {
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
                if sink.send(TransportMsg::Binary(out)).await.is_err() {
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
                    TransportMsg::Binary(mut b) => {
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
                    TransportMsg::Text(_) | TransportMsg::Ping(_) | TransportMsg::Pong(_) => {}
                    TransportMsg::Close(_) => break,
                }
            }
        }
    }
    Ok(())
}
