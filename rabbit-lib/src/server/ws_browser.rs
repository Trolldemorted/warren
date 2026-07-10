use crate::server::handle::AgentHandle;
use crate::server::registry::AgentRegistry;
use crate::server::transport::TransportMsg;
use crate::server::WsTransport;
use crate::wire::{Envelope, EnvelopeBody, TermFrame, PROTOCOL_VERSION, TERM_CHAN_CLAUDE};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use uuid::Uuid;

/// How often the server pings a browser WS to keep it alive through
/// reverse proxies / load balancers / TLS-terminating ingress. axum's
/// `WebSocketUpgrade` does NOT ship a default heartbeat; without this
/// ping, intermediaries close the connection at their idle timeout,
/// the browser sees `onclose`, and the page "flickers" as it
/// reconnects with exponential backoff. 20s is comfortably below the
/// common 60s idle-timeout floor and high enough that the extra
/// frames are noise on the wire.
const BROWSER_WS_PING_INTERVAL: Duration = Duration::from_secs(20);

/// Framework-agnostic browser-side session loop. Public so the
/// `rabbit-lib-axum` adapter can call it after wrapping an axum
/// `WebSocket` as a `DynWsTransport`. Kept in `rabbit-lib` because the
/// logic (term/meta replay, viewer-mode drop) is framework-free.
pub async fn handle(
    transport: impl WsTransport,
    registry: AgentRegistry,
    agent_id: Uuid,
    viewer_mode: bool,
) -> anyhow::Result<()> {
    // Split the transport first so the wait-for-arrival guard below can
    // observe early client closes without burning the upgrade — once
    // we await `notified()` blindly, the only way out of that future
    // is the registry firing or the client going away. Detecting the
    // latter requires the read half to be polled concurrently with
    // the notifier.
    let (mut sink, mut stream) = transport.split();

    // If no rabbit has registered yet, hold the WS open and wait for
    // one. Without this gate the handle() would `Err` immediately,
    // the server would close the WS, and the browser would reconnect
    // every ~500 ms — flipping the offline overlay and the empty
    // xterm at 1 Hz ("screen flickers about once a second"). Holding
    // the WS open and proceeding only when the registry fires keeps
    // the page in a single stable state.
    let handle = loop {
        if let Some(h) = registry.get(&agent_id) {
            break h.clone();
        }
        let mut notified = std::pin::pin!(registry.wait_for_arrival());
        tokio::select! {
            // Note: `bias` would be nice here, but we want to react
            // to a client-side close as quickly as possible — never
            // park for the full notify cycle if the user closed the
            // tab.
            _ = notified.as_mut() => continue,
            _msg = stream.next() => {
                // Either `None` (stream closed) or `Some(Err(_))`
                // (transport error) or `Some(Ok(TransportMsg::Close(_)))`
                // — all of these mean the client gave up. We don't
                // need to inspect the message to know to drop the
                // upgrade path.
                return Ok(());
            }
        }
    };
    let mut term_rx = handle.subscribe_term();
    let mut meta_rx = handle.subscribe_meta();

    for TermFrame { chan, seq, data } in handle.replay_term() {
        // §A.7: replay re-emits each frame verbatim with the seq that
        // rabbit assigned it. The browser uses that seq to trim buffered
        // live frames on a late-arriving `ScreenSnapshot::after_seq`. The
        // /shell endpoint shares the same broadcast channel — its own
        // ws_shell handler subscribes only to TERM_CHAN_SHELL.
        if chan != TERM_CHAN_CLAUDE {
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
    // §D Milestone 5 (Phase C): ask rabbit for an authoritative `ScreenSnapshot`
    // after flushing the bounded replay buffer. The browser's `applyMeta`
    // resets xterm.js and paints the snapshot precisely — replacing the v1
    // 250 ms sleep + SIGWINCH jiggle that used to live here. That jiggle
    // was a heuristic to coerce claude into redrawing for late joiners; the
    // server-side VT snapshot is exact, so the jiggle is gone.
    let snapshot_after = handle.clone();
    tokio::spawn(async move {
        if let Err(e) = snapshot_after.snapshot_request(TERM_CHAN_CLAUDE).await {
            log::debug!("snapshot request failed for agent {}: {e:?}", agent_id);
        }
    });

    // Heartbeat: the WS has no application-level keepalive by default,
    // so any reverse proxy in front of warren (the user is on
    // warren-patrician3.stronk.pw, almost certainly behind TLS-
    // terminating ingress) will close the connection at its idle
    // timeout. The browser then sees `onclose`, reconnects with
    // exponential backoff, and the screen visibly flickers. Pinging
    // every 20s keeps the path active without flooding the wire.
    let mut ping_interval = tokio::time::interval(BROWSER_WS_PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            chunk = term_rx.recv() => {
                let frame = match chunk {
                    Ok(f) => f,
                    Err(_) => break,
                };
                // §A.7: warren is a dumb pipe for the term stream — the
                // actor hands us a `TermFrame { chan, seq, data }`, we
                // re-emit the same shape on the browser socket so the
                // browser can match live frames against the snapshot's
                // `after_seq` and drop the ones already covered by the
                // snapshot grid.
                let TermFrame { chan, seq, data } = frame;
                if chan != TERM_CHAN_CLAUDE {
                    // Shell frames have their own WS endpoint.
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
            ev = meta_rx.recv() => {
                match ev {
                    Ok(body) => {
                        let env = Envelope {
                            v: PROTOCOL_VERSION,
                            seq: 0,
                            body,
                        };
                        if let Ok(s) = serde_json::to_string(&env) {
                            if sink.send(TransportMsg::Text(s)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = stream.next() => {
                let Some(msg) = msg else { break; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    TransportMsg::Text(t) => {
                        if let Ok(env) = serde_json::from_str::<Envelope>(&t) {
                            if let Err(e) = forward_browser_message(
                                &handle,
                                env,
                                viewer_mode,
                            ).await {
                                log::debug!("forward failed: {e:?}");
                            }
                        }
                    }
                    TransportMsg::Binary(mut b) => {
                        if b.is_empty() { continue; }
                        let chan = b.remove(0);
                        // The Claude channel carries raw bytes typed into the
                        // xterm.js pane. They must reach claude's PTY, not be
                        // re-broadcast to other viewers. The actor re-prepends
                        // the channel byte on the way to rabbit.
                        //
                        // §D read-only viewer mode: drop typed bytes for viewer
                        // connections. The JS already gates `term.onData` on
                        // `viewerMode`, but a hostile client could still send
                        // binary frames — server-side enforcement is the actual
                        // contract.
                        if chan == 0x01 && !viewer_mode {
                            if let Err(e) =
                                handle.send_terminal_bytes(
                                    TERM_CHAN_CLAUDE,
                                    Bytes::from(b),
                                ).await
                            {
                                log::debug!("send_terminal_bytes failed: {e:?}");
                            }
                        }
                    }
                    TransportMsg::Close(_) => break,
                    // Incoming Pings get a Pong reply automatically at the
                    // tungstenite protocol layer; we drop both Ping and
                    // Pong from the application loop (their only purpose
                    // here is keepalive, which we drive ourselves in the
                    // 4th select arm below).
                    TransportMsg::Ping(_) | TransportMsg::Pong(_) => {}
                }
            }
            _ = ping_interval.tick() => {
                // Server-initiated heartbeat. axum/tungstenite does NOT
                // ship a default keepalive, so without this arm the
                // connection dies at the first intermediary idle
                // timeout. An empty payload is fine — the protocol
                // allows arbitrary application data, and the peer only
                // needs the frame header to refresh the proxy's
                // activity timer.
                if sink.send(TransportMsg::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// True iff `body` is an *input* envelope that should be dropped when the
/// WS is in read-only viewer mode. Extracted for testability — the
/// `forward_browser_message` consumer uses this as the gate.
fn should_drop_for_viewer(body: &EnvelopeBody) -> bool {
    matches!(
        body,
        EnvelopeBody::Prompt { .. }
            | EnvelopeBody::Interrupt
            | EnvelopeBody::Slash { .. }
            | EnvelopeBody::Clear { .. }
            | EnvelopeBody::Resize { .. }
            | EnvelopeBody::Repaint
            | EnvelopeBody::Restart { .. }
    )
}

async fn forward_browser_message(
    handle: &AgentHandle,
    env: Envelope,
    viewer_mode: bool,
) -> anyhow::Result<()> {
    // §D read-only viewer mode: drop input frames unconditionally when
    // viewer_mode is on. Even though the JS template hides the input
    // affordances, the WS itself can still receive any envelope, so the
    // server is the last line of defense.
    if viewer_mode && should_drop_for_viewer(&env.body) {
        return Ok(());
    }
    match env.body {
        EnvelopeBody::Prompt {
            text,
            by_connection_id,
            ..
        } => {
            // Reject empty prompts at this boundary so they never reach
            // the actor's busy-gate or get injected into the PTY. A
            // browser keystroke like a stray Enter produces an empty
            // string today; without this guard the busy-gate would
            // busy-loop on the same empty text and the operator would
            // see repeated `PromptEcho` round-trips. Drop silently —
            // there's no envelope to reply to (the busy-gate echo is
            // reserved for stateful rejections, and an empty prompt
            // isn't a state change).
            if text.trim().is_empty() {
                log::debug!("dropping empty browser prompt");
                return Ok(());
            }
            // Browser tabs no longer carry their own connection id; the
            // server treats every prompt as "everyone's". The
            // `by_connection_id` field stays on the wire for protocol
            // symmetry with the prompt-rejection echo (and for future
            // HTTP / bg-task callers who might still want to stamp one).
            let _ = by_connection_id;
            handle.prompt(&text, false).await?;
        }
        EnvelopeBody::Interrupt => handle.interrupt().await?,
        EnvelopeBody::Clear { hard } => handle.clear(hard).await?,
        EnvelopeBody::Resize { cols, rows } => {
            handle.resize(cols, rows).await?;
        }
        EnvelopeBody::Repaint => {
            handle.repaint().await?;
        }
        EnvelopeBody::Restart { fresh } => handle.restart(fresh).await?,
        // Everything else is either output flowing server→browser (and
        // a browser shouldn't send it back), or a frame that has no
        // browser-side actor (e.g. `TuiConfig` is server→rabbit only).
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Envelope;

    #[test]
    fn viewer_drops_prompt_frame() {
        assert!(should_drop_for_viewer(&EnvelopeBody::Prompt {
            id: Uuid::new_v4(),
            text: "x".into(),
            by: "t".into(),
            by_connection_id: None,
        }));
    }

    #[test]
    fn viewer_drops_all_control_input_frames() {
        // Every control frame that would mutate the agent's PTY must be
        // dropped in viewer mode — a viewer can observe but not steer.
        for body in [
            EnvelopeBody::Interrupt,
            EnvelopeBody::Slash {
                cmd: "usage".into(),
            },
            EnvelopeBody::Clear { hard: false },
            EnvelopeBody::Resize { cols: 80, rows: 24 },
            EnvelopeBody::Repaint,
            EnvelopeBody::Restart { fresh: false },
        ] {
            assert!(
                should_drop_for_viewer(&body),
                "expected viewer to drop {body:?}"
            );
        }
    }

    #[test]
    fn viewer_does_not_drop_rejection_outcomes() {
        // `PromptRejected` is an *output* event from rabbit; it should
        // never appear as an inbound frame from the browser, but if it
        // somehow did, the viewer-mode gate must not strip it (output
        // frames must always pass).
        assert!(!should_drop_for_viewer(&EnvelopeBody::PromptRejected {
            id: Uuid::new_v4(),
            reason: "x".into(),
            by_connection_id: None,
        }));
    }

    #[test]
    fn viewer_does_not_drop_meta_state_or_usage() {
        // State / Usage / Session / Cleared / Log etc. are output frames
        // flowing rabbit → warren → browser. They are never inbound, but
        // the drop-list must not match them either, so a future shape
        // change can't accidentally silence output for viewers.
        for body in [EnvelopeBody::Pong, EnvelopeBody::Cleared { hard: false }] {
            assert!(
                !should_drop_for_viewer(&body),
                "output frame {body:?} must not be matched by viewer drop-list"
            );
        }
    }

    /// §Reject empty payloads: the browser inbound boundary drops
    /// empty / whitespace-only `Prompt.text` envelopes before they reach
    /// the actor. Without this guard, a stray Enter keystroke (or a
    /// buggy client) would push an empty prompt into the busy-gate,
    /// which would busy-loop on the same empty text and spam the
    /// operator with `PromptEcho` round-trips.
    #[tokio::test]
    async fn empty_prompt_text_is_dropped_at_browser_boundary() {
        use crate::server::actor::Command;
        use crate::server::handle::AgentHandle;
        let (cmd_tx, mut rx) = tokio::sync::mpsc::channel::<Command>(8);
        let handle = AgentHandle::with_cmd_tx(Uuid::nil(), cmd_tx);

        for empty_text in ["", "   ", "\n\t  \n"] {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Prompt {
                    id: Uuid::new_v4(),
                    text: empty_text.into(),
                    by: "browser".into(),
                    by_connection_id: None,
                },
            };
            forward_browser_message(&handle, env, false)
                .await
                .expect("drop is Ok");
        }
        // No command must have arrived — every empty prompt was
        // short-circuited at the boundary. `try_recv` is non-blocking
        // and returns Empty when the channel has nothing pending.
        let next = rx.try_recv();
        assert!(
            matches!(next, Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
            "empty prompts must not produce a Command; got {next:?}"
        );
    }
}