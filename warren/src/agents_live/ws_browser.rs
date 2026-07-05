use crate::agents_live::actor::Command;
use crate::agents_live::wire::{
    Envelope, EnvelopeBody, PROTOCOL_VERSION, TERM_CHAN_CLAUDE,
};
use crate::agents_live::AgentHandle;
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

/// §D read-only viewer mode: query params for `ws_browser`. When
/// `viewer=true`, the server drops every inbound input frame from this WS
/// (Prompts, Interrupts, Slash, Clear, Resize, Repaint, Restart) and every
/// typed terminal byte, regardless of what the browser JS sends. The UI
/// template also hides its input affordances, but the server-side drop is
/// the actual contract — client-side enforcement is cosmetic.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WsBrowserQuery {
    #[serde(default)]
    pub viewer: Option<bool>,
}

pub async fn ws_browser(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(q): Query<WsBrowserQuery>,
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
            log::debug!("browser ws closed for agent {}: {e:?}", id);
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
    let mut meta_rx = handle.subscribe_meta();

    // §A.6 leader-based resize: every browser tab gets a stable
    // `connection_id` so it can identify itself in subsequent
    // `ClaimLeader` / `ReleaseLeader` / `Resize` frames. Generated once
    // per WS upgrade; sent to this tab verbatim in `ConnectionAssigned`.
    let connection_id: Uuid = Uuid::new_v4();

    let (mut sink, mut stream) = socket.split();

    // §A.6: send `ConnectionAssigned` directly on this WS rather than via
    // `meta_tx` — only *this* tab needs to learn its own id. The browser
    // JS uses it to populate `myConnectionId`, which drives the
    // `Claim control` button visibility and the `window.resize` gate.
    let assigned_env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 0,
        body: EnvelopeBody::ConnectionAssigned { connection_id },
    };
    if let Ok(s) = serde_json::to_string(&assigned_env) {
        if sink.send(Message::Text(s)).await.is_err() {
            // Peer went away before we even told it its id; nothing more
            // to do.
            return Ok(());
        }
    }

    for chunk in handle.replay_term() {
        // Frame already includes the channel byte (actor passes it through);
        // filter to the Claude channel so this WS doesn't get shell bytes.
        if chunk.first() != Some(&TERM_CHAN_CLAUDE) {
            continue;
        }
        if sink.send(Message::Binary(chunk.to_vec())).await.is_err() {
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
        if let Err(e) = snapshot_after
            .snapshot_request(TERM_CHAN_CLAUDE)
            .await
        {
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
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(_) => break,
                };
                // §D multi-channel: the actor passes the channel byte
                // through. Forward only Claude-channel frames; shell
                // frames have their own WS endpoint.
                if bytes.first() != Some(&TERM_CHAN_CLAUDE) {
                    continue;
                }
                if sink.send(Message::Binary(bytes.to_vec())).await.is_err() {
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
                            if sink.send(Message::Text(s)).await.is_err() {
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
                    Message::Text(t) => {
                        if let Ok(env) = serde_json::from_str::<Envelope>(&t) {
                            if let Err(e) = forward_browser_message(
                                &handle,
                                env,
                                viewer_mode,
                                connection_id,
                            ).await {
                                log::debug!("forward failed: {e:?}");
                            }
                        }
                    }
                    Message::Binary(mut b) => {
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
                    Message::Close(_) => break,
                    // Incoming Pings get a Pong reply automatically at the
                    // tungstenite protocol layer; we drop both Ping and
                    // Pong from the application loop (their only purpose
                    // here is keepalive, which we drive ourselves in the
                    // 4th select arm below).
                    Message::Ping(_) | Message::Pong(_) => {}
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
                if sink.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }
    // §A.6 leader-based resize: fire a `ConnectionClosed` so the actor
    // can clear leadership if *this* tab was the leader. The leader
    // state itself lives on `AgentHandle`; the actor wraps the clear
    // with the `LeaderChanged { None }` broadcast.
    if let Err(e) = handle
        .cmd_tx()
        .send(Command::ConnectionClosed { connection_id })
        .await
    {
        // Channel full or actor gone (e.g. rabbit disconnected). The
        // leader field will eventually go stale; that's acceptable —
        // there's no auto-promotion, a fresh claim will overwrite it.
        log::debug!("connection_closed send failed for {connection_id}: {e:?}");
    }
    Ok(())
}

/// §A.6: per-tab leader state. Extracted so the routing logic in
/// `forward_browser_message` can be unit-tested without a real WS — the
/// caller passes a `connection_id` (whatever the WS assigned via
/// `ConnectionAssigned`) and the handle's current leadership state.
///
/// `claimed_activate` and `claimed_release` are fire-and-forget
/// commands: they don't await a reply because the actor broadcasts the
/// `LeaderChanged` envelope on its own. If the actor's command channel
/// is full we drop the command with a debug log — a stale leader claim
/// is much better than blocking the WS.
async fn send_cmd(handle: &AgentHandle, cmd: Command) {
    if let Err(e) = handle.cmd_tx().send(cmd).await {
        log::debug!("leader cmd send failed: {e:?}");
    }
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
            | EnvelopeBody::ClaimLeader { .. }
            | EnvelopeBody::ReleaseLeader
    )
}

async fn forward_browser_message(
    handle: &AgentHandle,
    env: Envelope,
    viewer_mode: bool,
    connection_id: Uuid,
) -> anyhow::Result<()> {
    // §D read-only viewer mode: drop input frames unconditionally when
    // viewer_mode is on. Even though the JS template hides the input
    // affordances, the WS itself can still receive any envelope, so the
    // server is the last line of defense.
    if viewer_mode && should_drop_for_viewer(&env.body) {
        return Ok(());
    }
    match env.body {
        EnvelopeBody::Prompt { text, .. } => {
            handle.prompt(&text, false).await?;
        }
        EnvelopeBody::Interrupt => handle.interrupt().await?,
        EnvelopeBody::Clear { hard } => handle.clear(hard).await?,
        EnvelopeBody::Resize { cols, rows } => {
            // §A.6: a non-leader's resize is dropped at this boundary.
            // The actor's `Command::ResizeFromConnection` arm
            // double-checks `is_leader` too as defense in depth.
            if !handle.is_leader(connection_id) {
                log::debug!("dropping Resize from non-leader {connection_id}");
                return Ok(());
            }
            send_cmd(
                handle,
                Command::ResizeFromConnection {
                    connection_id,
                    cols,
                    rows,
                },
            )
            .await;
        }
        EnvelopeBody::Repaint => {
            send_cmd(handle, Command::Repaint).await;
        }
        EnvelopeBody::Restart { fresh } => handle.restart(fresh).await?,
        EnvelopeBody::ClaimLeader { cols, rows } => {
            // §A.6: claims always succeed. The actor overwrites the prior
            // leader (even if still connected) and broadcasts the new
            // identity to every browser.
            send_cmd(
                handle,
                Command::ClaimLeader {
                    connection_id,
                    cols,
                    rows,
                },
            )
            .await;
        }
        EnvelopeBody::ReleaseLeader => {
            send_cmd(handle, Command::ReleaseLeader { connection_id }).await;
        }
        // ConnectionAssigned / LeaderChanged are output frames flowing
        // server→browser; if a hostile client sends one back we silently
        // ignore (no side-effects).
        EnvelopeBody::ConnectionAssigned { .. } | EnvelopeBody::LeaderChanged { .. } => {}
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents_live::wire::Envelope;

    #[test]
    fn viewer_drops_prompt_frame() {
        assert!(should_drop_for_viewer(&EnvelopeBody::Prompt {
            id: Uuid::new_v4(),
            text: "x".into(),
            by: "t".into(),
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
            EnvelopeBody::Resize {
                cols: 80,
                rows: 24,
            },
            EnvelopeBody::Repaint,
            EnvelopeBody::Restart { fresh: false },
            EnvelopeBody::ClaimLeader {
                cols: 80,
                rows: 24,
            },
            EnvelopeBody::ReleaseLeader,
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
        }));
    }

    #[test]
    fn viewer_does_not_drop_meta_state_or_usage() {
        // State / Usage / Session / Cleared / Log etc. are output frames
        // flowing rabbit → warren → browser. They are never inbound, but
        // the drop-list must not match them either, so a future shape
        // change can't accidentally silence output for viewers.
        for body in [
            EnvelopeBody::Pong,
            EnvelopeBody::Cleared { hard: false },
        ] {
            assert!(
                !should_drop_for_viewer(&body),
                "output frame {body:?} must not be matched by viewer drop-list"
            );
        }
    }

    /// Compose an `Envelope` for testing only — uses the wire-side
    /// `Envelope` shape so the tests mirror what the browser sends.
    fn make_env(body: EnvelopeBody) -> Envelope {
        Envelope {
            v: PROTOCOL_VERSION,
            seq: 0,
            body,
        }
    }

    // §A.6: leader-aware routing. We drive `forward_browser_message`
    // against a real `AgentHandle` whose cmd_tx is plumbed into a fresh
    // mpsc::Receiver, and assert that the right `Command` arrives. The
    // ws_sink / ws_stream half of the WS doesn't matter here — only the
    // routing decision does.

    use crate::agents_live::handle::AgentHandle;

    /// Build an `AgentHandle` whose cmd_tx we can introspect: every
    /// command the actor-side helper would have received shows up on
    /// `rx`.
    fn handle_with_cmd_rx() -> (AgentHandle, tokio::sync::mpsc::Receiver<Command>) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let handle = AgentHandle::with_cmd_tx(Uuid::nil(), tx);
        (handle, rx)
    }

    #[tokio::test]
    async fn claim_leader_text_envelope_routes_to_command() {
        let (handle, mut rx) = handle_with_cmd_rx();
        let me = Uuid::from_bytes([1; 16]);
        let env = make_env(EnvelopeBody::ClaimLeader {
            cols: 120,
            rows: 40,
        });
        forward_browser_message(&handle, env, false, me)
            .await
            .expect("forward");
        let got = rx.recv().await.expect("a command must arrive");
        match got {
            Command::ClaimLeader {
                connection_id,
                cols,
                rows,
            } => {
                assert_eq!(connection_id, me);
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            other => panic!("expected ClaimLeader, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn release_leader_text_envelope_routes_to_command() {
        let (handle, mut rx) = handle_with_cmd_rx();
        let me = Uuid::from_bytes([2; 16]);
        let env = make_env(EnvelopeBody::ReleaseLeader);
        forward_browser_message(&handle, env, false, me)
            .await
            .expect("forward");
        let got = rx.recv().await.expect("a command must arrive");
        match got {
            Command::ReleaseLeader { connection_id } => {
                assert_eq!(connection_id, me);
            }
            other => panic!("expected ReleaseLeader, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leader_resize_accepted() {
        let (handle, mut rx) = handle_with_cmd_rx();
        let me = Uuid::from_bytes([3; 16]);
        handle.claim_leader(me, 120, 40);
        let env = make_env(EnvelopeBody::Resize {
            cols: 132,
            rows: 50,
        });
        forward_browser_message(&handle, env, false, me)
            .await
            .expect("forward");
        let got = rx.recv().await.expect("a command must arrive");
        match got {
            Command::ResizeFromConnection {
                connection_id,
                cols,
                rows,
            } => {
                assert_eq!(connection_id, me);
                assert_eq!(cols, 132);
                assert_eq!(rows, 50);
            }
            other => panic!("expected ResizeFromConnection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_leader_resize_dropped_at_input() {
        let (handle, mut rx) = handle_with_cmd_rx();
        let leader = Uuid::from_bytes([4; 16]);
        let me = Uuid::from_bytes([5; 16]);
        handle.claim_leader(leader, 120, 40);
        assert!(!handle.is_leader(me));
        let env = make_env(EnvelopeBody::Resize {
            cols: 100,
            rows: 30,
        });
        forward_browser_message(&handle, env, false, me)
            .await
            .expect("forward");
        // No command should have arrived — the resize was dropped at the
        // ws_browser boundary. `try_recv` returns Err(Empty) when the
        // channel is empty (no blocking).
        let next = rx.try_recv();
        assert!(
            matches!(next, Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
            "non-leader resize must not produce a command; got {next:?}"
        );
    }

    #[tokio::test]
    async fn viewer_drops_claim_and_release_frames() {
        // Even in viewer mode, the claim/release flows go through the
        // viewer-mode drop gate — a viewer must not be able to
        // manipulate leadership either.
        let (handle, mut rx) = handle_with_cmd_rx();
        let me = Uuid::from_bytes([6; 16]);
        for env in [
            make_env(EnvelopeBody::ClaimLeader {
                cols: 120,
                rows: 40,
            }),
            make_env(EnvelopeBody::ReleaseLeader),
        ] {
            forward_browser_message(&handle, env, true, me)
                .await
                .expect("forward");
        }
        let next = rx.try_recv();
        assert!(
            matches!(next, Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
            "viewer_mode must drop claim/release; got {next:?}"
        );
    }

    #[tokio::test]
    async fn connection_assigned_and_leader_changed_inbound_silently_ignored() {
        // These are server-→-browser output frames. A hostile client
        // sending them inbound must not produce a command.
        let (handle, mut rx) = handle_with_cmd_rx();
        let me = Uuid::from_bytes([7; 16]);
        for env in [
            make_env(EnvelopeBody::ConnectionAssigned {
                connection_id: Uuid::from_bytes([8; 16]),
            }),
            make_env(EnvelopeBody::LeaderChanged {
                leader_id: Some(Uuid::from_bytes([9; 16])),
                cols: 80,
                rows: 24,
            }),
        ] {
            forward_browser_message(&handle, env, false, me)
                .await
                .expect("forward");
        }
        let next = rx.try_recv();
        assert!(
            matches!(next, Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
            "inbound ConnectionAssigned/LeaderChanged must be silently ignored; got {next:?}"
        );
    }
}
