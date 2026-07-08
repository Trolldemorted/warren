use crate::server::handle::AgentHandle;
use crate::server::handle::AgentStateSnapshot;
use crate::server::transport::{TransportMsg, WsTransport};
use crate::server::SessionStore;
use crate::wire::{
    AgentState, Envelope, EnvelopeBody, HelloDown, TermFrame, TermSize, UsageSnapshot,
    PROTOCOL_VERSION,
};
use anyhow::Result;
use bytes::Bytes;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// `wait=true` prompts queue here waiting for `StopHook`. `wait=false`
/// callers never enter the queue.
const PENDING_CAP: usize = 16;

#[derive(Debug)]
pub enum Command {
    Prompt {
        id: Uuid,
        text: String,
        by: String,
        wait: bool,
        reply: Option<oneshot::Sender<TurnOutcomeMsg>>,
        /// §Cross-tab prompt rejection visibility: the originating
        /// browser's connection id. The actor stamps this onto the
        /// downstream `EnvelopeBody::Prompt` and onto the
        /// `PromptRejected` envelope it publishes when the busy/queue
        /// gates fire. `None` for HTTP / bg-task (the rejection
        /// banner is treated as "everyone" by browsers).
        by_connection_id: Option<Uuid>,
    },
    Clear {
        hard: bool,
        reply: Option<oneshot::Sender<()>>,
    },
    Compact,
    Interrupt,
    /// §Usage-limits: triggered by `POST /api/agents/:id/claude/usage_check`.
    /// The actor sends `EnvelopeBody::UsageCheck` to rabbit over the
    /// existing WS link; the rabbit supervisor runs the synchronous
    /// `/usage` scrape (write `\x15/usage\r`, drain ~2s of PTY bytes,
    /// parse with `observer::limits::LimitsParser`, send single Esc to
    /// dismiss the overlay) and publishes the parsed `Usage` envelope
    /// back through the link. The HTTP handler returns 202 Accepted
    /// immediately — the parsed data arrives on the SSE
    /// `/events/stream` channel a moment later.
    UsageCheck,
    Restart {
        fresh: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Ask rabbit to force a full TUI repaint by emitting two SIGWINCHs
    /// (jiggle by one column, settle, restore). Used after a late-join
    /// replay buffer has landed in a fresh xterm.js pane.
    Repaint,
    /// Raw bytes typed into a terminal pane, tagged with the channel they
    /// belong to (`TERM_CHAN_CLAUDE` for the claude pane, `TERM_CHAN_SHELL`
    /// for the `/shell` pane). The actor prepends `chan` on the wire so rabbit
    /// routes them to the right PTY.
    SendKeys {
        chan: u8,
        data: Bytes,
        /// `Some(id)` triggers the leader-gate check on dispatch;
        /// `None` skips it.
        connection_id: Option<Uuid>,
    },
    /// §D Milestone 5 (Phase B): ask rabbit for a current `ScreenSnapshot`
    /// of the given channel. Sent by the browser WS right after flushing
    /// the bounded replay buffer; rabbit responds with a `ScreenSnapshot`
    /// envelope that the browser applies verbatim.
    SnapshotRequest {
        chan: u8,
    },
    // §A.6 leader-based resize ------------------------------------------
    /// Browser tab asks to claim leadership for `connection_id` at
    /// `(cols, rows)`. Claims always succeed (transfers from a prior
    /// leader if one is set). The actor broadcasts `LeaderChanged` to all
    /// browsers and, if the claimed size differs from the current PTY size,
    /// pushes a `Resize` envelope to rabbit.
    ClaimLeader {
        connection_id: Uuid,
        cols: u16,
        rows: u16,
    },
    /// Leader voluntarily releases control. No-op if a different
    /// connection holds leadership. On success the actor broadcasts
    /// `LeaderChanged { leader_id: None, ... }`.
    ReleaseLeader {
        connection_id: Uuid,
    },
    /// Browser tab asks to resize the kernel PTY to `(cols, rows)`. Only
    /// forwarded to rabbit when `connection_id` matches the current leader
    /// — non-leader resizes are dropped at the `ws_browser.rs` boundary
    /// before they reach this command, but a defense-in-depth check is
    /// worth keeping here too.
    ResizeFromConnection {
        connection_id: Uuid,
        cols: u16,
        rows: u16,
    },
    /// Browser WS closed (any reason — graceful close, network drop, or
    /// server-side teardown). If the closing connection was the leader,
    /// the actor clears leadership and broadcasts `LeaderChanged { None }`.
    /// No auto-promotion: a new leader must explicitly claim.
    ConnectionClosed {
        connection_id: Uuid,
    },
}

#[derive(Debug, Clone)]
pub struct TurnOutcomeMsg {
    pub prompt_id: Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub usage: Option<UsageSnapshot>,
    pub error: Option<String>,
}

pub async fn run(
    store: Arc<dyn SessionStore>,
    handle: AgentHandle,
    agent_id: Uuid,
    socket: impl WsTransport + 'static,
    cmd_rx: mpsc::Receiver<Command>,
) -> Result<()> {
    let join = tokio::spawn(run_inner(store, handle, agent_id, socket, cmd_rx));
    join.await.map_err(|e| anyhow::anyhow!("actor join: {e}"))?;
    Ok(())
}

async fn run_inner<T: WsTransport>(
    store: Arc<dyn SessionStore>,
    handle: AgentHandle,
    agent_id: Uuid,
    socket: T,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    let (mut sink, mut stream) = socket.split();

    let hello = match read_hello(&mut stream).await {
        Ok(h) => h,
        Err(e) => {
            log::warn!("actor hello read failed: {e:?}");
            return;
        }
    };

    // Resume seq past the highest row we already persisted for this agent.
    // Hello takes the first free seq; subsequent messages advance from there.
    // Without this, every reconnect would try to insert seq=1 again and
    // violate the (agent_id, seq) unique index.
    let mut seq: i64 = match store.next_event_seq(agent_id).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("next_event_seq failed for {agent_id}: {e:?}");
            return;
        }
    };
    // Persist the hello BEFORE publishing the new state to subscribers.
    // The DB row is the source of truth for "what happened"; the meta
    // broadcast is local and best-effort. If the insert fails we'd rather
    // have no row AND no broadcast than a row-less broadcast that misleads
    // SSE listeners.
    persist_event(
        &*store,
        agent_id,
        &serde_json::to_value(&hello).unwrap_or(serde_json::Value::Null),
        "hello",
        seq,
    )
    .await
    .ok();
    seq += 1;

    handle.update_state(AgentStateSnapshot {
        state: hello.state,
        session_id: hello.session_id.clone(),
        claude_version: Some(hello.claude_version.clone()),
        last_usage: UsageSnapshot {
            source: "transcript".to_string(),
            ..Default::default()
        },
        // §A.6 leader-based resize: capture the boot-time PTY size from
        // the Hello envelope. Refreshed later when a leader-driven Resize
        // flows through; the broadcast `LeaderChanged { leader_id: None }`
        // uses this as the (cols, rows) payload so followers always see
        // a real grid (never 0×0).
        term_size: Some(hello.term_size),
    });

    let mut pending: std::collections::VecDeque<(Uuid, oneshot::Sender<TurnOutcomeMsg>)> =
        std::collections::VecDeque::new();
    let mut started_at: HashMap<Uuid, chrono::DateTime<Utc>> = HashMap::new();
    let mut last_usage = UsageSnapshot {
        source: "transcript".to_string(),
        ..Default::default()
    };
    // Ack bookkeeping: we send EnvelopeBody::Ack{highest_persisted_seq}
    // back to rabbit periodically so its meta ring can trim. Cadence is
    // every ACK_BATCH events or every ACK_INTERVAL — whichever fires first.
    // We start by acking everything that already exists in the DB (seq - 1
    // after the hello persist above) so a reconnecting rabbit immediately
    // drops anything it had buffered from the previous session.
    let mut last_acked_seq: i64 = seq - 1;
    let mut events_since_ack: usize = 0;
    let mut last_ack_at: Instant = Instant::now();
    const ACK_BATCH: usize = 16;
    const ACK_INTERVAL: Duration = Duration::from_secs(2);
    // §Connection-lost surfacing: server-initiated Ping. axum/tungstenite
    // does NOT ship a default keepalive, so without this arm the rabbit
    // WS dies silently at the first intermediary idle timeout (a NAT or
    // load balancer can drop the flow without sending FIN/RST). An empty
    // Ping is enough — the protocol allows arbitrary application data
    // and the peer only needs the frame header to refresh the proxy's
    // activity timer. Mirrors `BROWSER_WS_PING_INTERVAL` in
    // `ws_browser.rs` so both surfaces drive heartbeats at the same
    // cadence.
    const RABBIT_WS_PING_INTERVAL: Duration = Duration::from_secs(20);
    let mut ack_ticker = tokio::time::interval(ACK_INTERVAL);
    ack_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately; we'd rather not ack-empty on tick 0
    // unless there's actually something to ack — check inside the loop.
    ack_ticker.tick().await;
    let mut ping_ticker = tokio::time::interval(RABBIT_WS_PING_INTERVAL);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Drop the immediate first tick — see ack_ticker above for the
    // same reasoning.
    ping_ticker.tick().await;
    // Send the initial ack for everything already in the DB. seq - 1 here
    // is the highest seq the hello was persisted at (it was incremented
    // right after the persist above).
    if last_acked_seq >= 0 {
        send_ack(&mut sink, last_acked_seq).await;
        last_ack_at = Instant::now();
    }

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                if let Err(e) = dispatch(cmd, &handle, &mut sink, &mut pending, &mut started_at).await {
                    log::warn!("dispatch error: {e:?}");
                }
            }
            msg = stream.next() => {
                let Some(msg) = msg else { break; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("ws recv error: {e:?}");
                        break;
                    }
                };
                match msg {
                    TransportMsg::Text(t) => {
                        let env: Envelope = match serde_json::from_str(&t) {
                            Ok(v) => v,
                            Err(e) => {
                                log::warn!("bad envelope from rabbit: {e:?}");
                                continue;
                            }
                        };
                        if let EnvelopeBody::State(s) = &env.body {
                            handle.update_state(AgentStateSnapshot {
                                state: s.state,
                                session_id: s.session_id.clone(),
                                claude_version: None,
                                last_usage: last_usage.clone(),
                                // State updates don't carry a fresh term_size;
                                // leave it None so `update_state` keeps the
                                // cached value sticky.
                                term_size: None,
                            });
                        }
                        if let EnvelopeBody::PromptEcho(pe) = &env.body {
                            started_at.insert(pe.prompt_id, Utc::now());
                        }
                        if let EnvelopeBody::StopHook { prompt_id, usage, error } = &env.body {
                            let actual_id = pending
                                .front()
                                .map(|(id, _)| *id)
                                .unwrap_or(*prompt_id);
                            let outcome = TurnOutcomeMsg {
                                prompt_id: actual_id,
                                started_at: started_at.remove(&actual_id).unwrap_or_else(Utc::now),
                                ended_at: Utc::now(),
                                usage: usage.clone(),
                                error: error.clone(),
                            };
                            if let Some(u) = usage {
                                last_usage = u.clone();
                            }
                            if let Some((_, tx)) = pending.pop_front() {
                                let _ = tx.send(outcome);
                            }
                        }
                        let payload_json = serde_json::to_value(&env).unwrap_or(serde_json::Value::Null);
                        let kind = envelope_kind(&env.body).to_string();
                        if matches!(&env.body, EnvelopeBody::Ack { .. }) {
                            // rabbit shouldn't ack warren; ignore if it does.
                            continue;
                        }
                        // Dedup: events replayed from a previous session
                        // arrive with seq <= (seq - 1), which is the highest
                        // we've already persisted. Persist returns Err on
                        // the unique-index collision, which we swallow.
                        if env.seq < seq {
                            log::debug!(
                                "skipping duplicate seq={} (already persisted up to {})",
                                env.seq,
                                seq - 1
                            );
                        } else {
                            persist_event(&*store, agent_id, &payload_json, &kind, seq)
                                .await
                                .ok();
                            seq += 1;
                            events_since_ack += 1;
                            if events_since_ack >= ACK_BATCH
                                || last_ack_at.elapsed() >= ACK_INTERVAL
                            {
                                let new_acked = seq - 1;
                                if new_acked > last_acked_seq {
                                    send_ack(&mut sink, new_acked).await;
                                    last_acked_seq = new_acked;
                                    events_since_ack = 0;
                                    last_ack_at = Instant::now();
                                }
                            }
                        }
                        handle.publish_meta(env.body);
                    }
                    TransportMsg::Binary(b) => {
                        // §A.7: server→browser terminal binary frames are
                        // now `<chan:1> <seq:8 BE> <data>`. Drop malformed
                        // frames (too short for the prelude) entirely —
                        // warren is a dumb pipe and would rather miss a
                        // frame than seed the broadcast with a partial
                        // seq that downstream panes interpret as
                        // "everything since seq=N has been delivered." A
                        // Rabbit that misroutes bytes would still land
                        // here; the prelude check is cheap (10-byte
                        // bound) and keeps the invariant auditable.
                        if b.len() < 10 { continue; }
                        let chan = b[0];
                        let mut seq_arr = [0u8; 8];
                        seq_arr.copy_from_slice(&b[1..9]);
                        let seq = u64::from_be_bytes(seq_arr);
                        handle.publish_term(TermFrame {
                            chan,
                            seq,
                            data: b[9..].to_vec(),
                        });
                    }
                    TransportMsg::Close(_) => break,
                    TransportMsg::Ping(_) | TransportMsg::Pong(_) => {}
                }
            }
            _ = ack_ticker.tick() => {
                let new_acked = seq - 1;
                if new_acked > last_acked_seq
                    && (events_since_ack > 0 || last_ack_at.elapsed() >= ACK_INTERVAL)
                {
                    send_ack(&mut sink, new_acked).await;
                    last_acked_seq = new_acked;
                    events_since_ack = 0;
                    last_ack_at = Instant::now();
                }
            }
            _ = ping_ticker.tick() => {
                // Server-initiated heartbeat — see RABBIT_WS_PING_INTERVAL
                // above. If the send fails the rabbit WS is gone; break
                // out so the actor publishes the offline state and the
                // link task can reconnect with jittered backoff.
                if sink.send(TransportMsg::Ping(Vec::new())).await.is_err() {
                    log::debug!("rabbit ws ping send failed; breaking actor loop");
                    break;
                }
            }
        }
    }

    // §Connection-lost surfacing: the rabbit WS died (stream EOF, recv
    // error, Close frame, ping send failure, or supervisor shutdown).
    // Publish `AgentState::Dead` so subscribers (browser WS, SSE
    // handlers, UI badge) flip to the offline affordance immediately.
    // On the next reconnect the new actor reads a fresh Hello and
    // overwrites this state with whatever the new Hello carries — so
    // the offline window is bounded by the reconnect (with jittered
    // backoff, see rabbit/src/link.rs).
    handle.update_state(AgentStateSnapshot {
        state: AgentState::Dead,
        session_id: None,
        claude_version: None,
        last_usage: UsageSnapshot {
            source: "transcript".to_string(),
            ..Default::default()
        },
        term_size: None,
    });
}

async fn send_ack<T: WsTransport>(
    sink: &mut futures_util::stream::SplitSink<T, TransportMsg>,
    ack_seq: i64,
) {
    let env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 0,
        body: EnvelopeBody::Ack { ack_seq },
    };
    if let Ok(s) = serde_json::to_string(&env) {
        if sink.send(TransportMsg::Text(s)).await.is_err() {
            log::debug!("ack send failed (sink closed)");
        }
    }
}

async fn read_hello<T: WsTransport>(
    stream: &mut futures_util::stream::SplitStream<T>,
) -> Result<HelloDown> {
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        if let TransportMsg::Text(t) = msg {
            let env: Envelope = serde_json::from_str(&t)?;
            if env.v != PROTOCOL_VERSION {
                anyhow::bail!("protocol mismatch: {}", env.v);
            }
            if let EnvelopeBody::Hello(h) = env.body {
                return Ok(h);
            }
        }
    }
    anyhow::bail!("no hello from rabbit")
}

async fn dispatch<T: WsTransport>(
    cmd: Command,
    handle: &AgentHandle,
    sink: &mut futures_util::stream::SplitSink<T, TransportMsg>,
    pending: &mut std::collections::VecDeque<(Uuid, oneshot::Sender<TurnOutcomeMsg>)>,
    started_at: &mut HashMap<Uuid, chrono::DateTime<Utc>>,
) -> Result<()> {
    match cmd {
        Command::Prompt {
            id,
            text,
            by,
            wait,
            reply,
            by_connection_id,
        } => {
            // Single-funnel gate: every prompt surface (HTTP, WS,
            // future bg-task schedulers) lands here.
            let snap = handle.snapshot();
            let reject_reason: Option<&'static str> = match snap.state {
                AgentState::Running => Some("agent is running a turn"),
                AgentState::Dead => Some("agent is dead"),
                _ => None,
            };
            if let Some(reason) = reject_reason {
                handle.publish_meta(EnvelopeBody::PromptRejected {
                    id,
                    reason: reason.to_string(),
                    by_connection_id,
                });
                if wait {
                    if let Some(tx) = reply {
                        let now = Utc::now();
                        let _ = tx.send(TurnOutcomeMsg {
                            prompt_id: id,
                            started_at: now,
                            ended_at: now,
                            usage: None,
                            error: Some(reason.to_string()),
                        });
                    }
                }
                return Ok(());
            }
            // Bounded queue: only `wait=true` callers enter `pending`.
            if wait && pending.len() >= PENDING_CAP {
                handle.publish_meta(EnvelopeBody::PromptRejected {
                    id,
                    reason: "turn queue full".to_string(),
                    by_connection_id,
                });
                if let Some(tx) = reply {
                    let now = Utc::now();
                    let _ = tx.send(TurnOutcomeMsg {
                        prompt_id: id,
                        started_at: now,
                        ended_at: now,
                        usage: None,
                        error: Some("turn queue full".to_string()),
                    });
                }
                return Ok(());
            }
            let started = Utc::now();
            started_at.insert(id, started);
            if wait {
                if let Some(tx) = reply {
                    pending.push_back((id, tx));
                }
            }
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Prompt {
                    id,
                    text,
                    by,
                    by_connection_id,
                },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Clear { hard, reply } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Clear { hard },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
            if let Some(tx) = reply {
                let _ = tx.send(());
            }
        }
        Command::Compact => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Slash {
                    cmd: "compact".to_string(),
                },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Interrupt => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Interrupt,
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::UsageCheck => {
            // §Usage-limits: send a UsageCheck envelope to rabbit; the
            // supervisor runs the synchronous `/usage` scrape and
            // publishes the parsed result back as an `EnvelopeBody::Usage`
            // carrying the new `weekly_pct` / `session_pct` fields. This
            // arm is fire-and-forget — the HTTP handler already returned
            // 202 Accepted to the browser; the parsed data arrives on
            // the SSE `/events/stream` channel via the meta plane.
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::UsageCheck,
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Restart { fresh } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Restart { fresh },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Resize { cols, rows } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Resize { cols, rows },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Repaint => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Repaint,
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::SendKeys {
            chan,
            data,
            connection_id,
        } => {
            // Defense-in-depth for the JS-side `isLeader` gate:
            // a hostile client could still send binary frames. Surface
            // the drop to the originating connection via an
            // `InputRejected` envelope so the browser can render a
            // banner; without it, keystrokes vanish silently and the
            // user has no idea why their input is being ignored.
            if let Some(cid) = connection_id {
                if !handle.is_leader(cid) {
                    log::debug!(
                        "dropping non-leader SendKeys (chan={chan}, {} bytes, conn={cid})",
                        data.len()
                    );
                    handle.publish_meta(EnvelopeBody::InputRejected {
                        reason: "another tab holds the keyboard".into(),
                        by_connection_id: Some(cid),
                    });
                    return Ok(());
                }
            }
            let mut frame = vec![chan];
            frame.extend_from_slice(&data);
            sink.send(TransportMsg::Binary(frame)).await?;
        }
        Command::SnapshotRequest { chan } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::SnapshotRequest { chan },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        // §A.6 leader-based resize — see `handle_leader_command` for the
        // full rationale. Each arm mutates `handle` (leader state, term_size
        // cache) and may push a `Resize` envelope to rabbit when the
        // claimed/dropped size differs from the current PTY size. The
        // browser-side `LeaderChanged` broadcast fires on every state
        // change (claim, release, disconnect) so followers can resize
        // their xterm grids accordingly.
        Command::ClaimLeader {
            connection_id,
            cols,
            rows,
        } => {
            // Read the *prior* term_size *before* mutating, so the
            // inherit-then-resize comparison is correct. After
            // `update_state` below the snapshot would already equal the
            // new size and the conditional below would never fire — that
            // bug would silently swallow every leader-driven Resize.
            let prior = handle
                .snapshot()
                .term_size
                .unwrap_or(TermSize { cols: 0, rows: 0 });
            // Claims always succeed — transfers from a prior leader if one
            // is connected. The bool return is informational; the broadcast
            // fires regardless.
            let _was_prior = handle.claim_leader(connection_id, cols, rows);
            // Refresh the cached term_size so the broadcast carries the
            // claimed size (the snapshot's pre-claim value would otherwise
            // persist).
            handle.update_state(AgentStateSnapshot {
                term_size: Some(TermSize { cols, rows }),
                ..AgentStateSnapshot::default()
            });
            handle.publish_meta(EnvelopeBody::LeaderChanged {
                leader_id: Some(connection_id),
                cols,
                rows,
            });
            // Inherit-then-resize: if the claimed size differs from the
            // pre-claim PTY size, push a Resize to rabbit so the kernel
            // winsize follows the new leader. On the first claim the
            // prior size is (0, 0) and we always push — desirable, since
            // the PTY needs its winsize set anyway.
            if prior.cols != cols || prior.rows != rows {
                let env = Envelope {
                    v: PROTOCOL_VERSION,
                    seq: 0,
                    body: EnvelopeBody::Resize { cols, rows },
                };
                sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                    .await?;
            }
        }
        Command::ReleaseLeader { connection_id } => {
            if handle.release_leader(connection_id) {
                let cur = handle
                    .snapshot()
                    .term_size
                    .unwrap_or(TermSize { cols: 0, rows: 0 });
                handle.publish_meta(EnvelopeBody::LeaderChanged {
                    leader_id: None,
                    cols: cur.cols,
                    rows: cur.rows,
                });
            }
        }
        Command::ResizeFromConnection {
            connection_id,
            cols,
            rows,
        } => {
            // Defense in depth — non-leader resizes should already be
            // dropped at the ws_browser.rs boundary.
            if !handle.is_leader(connection_id) {
                log::debug!("actor dropped ResizeFromConnection from non-leader {connection_id}");
                return Ok(());
            }
            // Refresh the cached size and forward to rabbit (no broadcast
            // — the leader's own browser already knows its own size).
            handle.update_state(AgentStateSnapshot {
                term_size: Some(TermSize { cols, rows }),
                ..AgentStateSnapshot::default()
            });
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Resize { cols, rows },
            };
            sink.send(TransportMsg::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::ConnectionClosed { connection_id } => {
            if handle.clear_leader_if(connection_id) {
                let cur = handle
                    .snapshot()
                    .term_size
                    .unwrap_or(TermSize { cols: 0, rows: 0 });
                handle.publish_meta(EnvelopeBody::LeaderChanged {
                    leader_id: None,
                    cols: cur.cols,
                    rows: cur.rows,
                });
            }
        }
    }
    Ok(())
}

fn envelope_kind(body: &EnvelopeBody) -> &'static str {
    match body {
        EnvelopeBody::Hello(_) => "hello",
        EnvelopeBody::Ack { .. } => "ack",
        EnvelopeBody::State(_) => "state",
        EnvelopeBody::PromptEcho(_) => "prompt_echo",
        EnvelopeBody::TurnDone(_) => "turn_done",
        EnvelopeBody::Usage(_) => "usage",
        EnvelopeBody::Cleared { .. } => "cleared",
        EnvelopeBody::Session(_) => "session",
        EnvelopeBody::TranscriptMsg { .. } => "transcript_msg",
        EnvelopeBody::Log(_) => "log",
        EnvelopeBody::Pong => "pong",
        EnvelopeBody::Prompt { .. } => "prompt",
        EnvelopeBody::Slash { .. } => "slash",
        EnvelopeBody::Interrupt => "interrupt",
        EnvelopeBody::Clear { .. } => "clear",
        EnvelopeBody::UsageCheck => "usage_check",
        EnvelopeBody::Restart { .. } => "restart",
        EnvelopeBody::Resize { .. } => "resize",
        EnvelopeBody::Repaint => "repaint",
        EnvelopeBody::StopHook { .. } => "stop_hook",
        EnvelopeBody::PromptRejected { .. } => "prompt_rejected",
        EnvelopeBody::ScreenSnapshot { .. } => "screen_snapshot",
        EnvelopeBody::SnapshotRequest { .. } => "snapshot_request",
        EnvelopeBody::ConnectionAssigned { .. } => "connection_assigned",
        EnvelopeBody::ClaimLeader { .. } => "claim_leader",
        EnvelopeBody::ReleaseLeader => "release_leader",
        EnvelopeBody::LeaderChanged { .. } => "leader_changed",
        EnvelopeBody::InputRejected { .. } => "input_rejected",
    }
}

async fn persist_event(
    store: &dyn SessionStore,
    agent_id: Uuid,
    payload: &serde_json::Value,
    kind: &str,
    seq: i64,
) -> Result<()> {
    // Serialize once into a String so the trait can stay FFI-shaped
    // (&str over the wire). For callers that already hold a `Value`
    // and want to skip the serialization, `insert_event_value` is the
    // escape hatch.
    let payload_json = serde_json::to_string(payload)
        .map_err(|e| anyhow::anyhow!("event payload serialize: {e}"))?;
    store
        .insert_event(agent_id, seq, kind, &payload_json)
        .await
        .map_err(|e| match e {
            crate::server::StoreError::Duplicate => {
                anyhow::anyhow!("event already persisted at seq {seq}")
            }
            other => anyhow::anyhow!("insert_event: {other}"),
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! §A.6 leader-based resize: actor-level dispatch tests.
    //!
    //! The `dispatch` function takes a generic
    //! `SplitSink<T: WsTransport, TransportMsg>`. End-to-end tests
    //! that exercise dispatch against a fake transport live in
    //! `transport::tests` (the `dyn_ws_transport_round_trips_every_variant`
    //! and `split_works_on_dyn_ws_transport` cases); the leader-state
    //! side-effects (claim/release/clear, the `LeaderChanged`
    //! broadcast) are fully covered here.
    //!
    //! The leader-state side-effects (claim/release/clear, the
    //! `LeaderChanged` broadcast) are fully covered by `handle.rs::tests`
    //! and the meta-broadcast assertions below. The remaining gap —
    //! asserting that a `Resize` envelope is sent to rabbit only when the
    //! claimed size differs — is pinned down at the handle level
    //! (`is_leader`, `update_leader_size`) and at the ws_browser level
    //! (the `Resize` drop-rule tests).

    use super::*;
    use crate::server::handle::AgentHandle;
    use crate::server::transport::CloseReason;
    use crate::server::{AgentEventRecord, SessionStore, StoreError};
    use crate::wire::HelloUp;
    use futures_util::{Sink, Stream};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    /// Minimal `SessionStore` for actor-level tests. Returns
    /// `next_event_seq = 1` so the first event persists at seq=1, and
    /// accepts every insert without trying to enforce uniqueness. The
    /// actor treats `Duplicate` as "already persisted" via `.ok()` and
    /// swallows other errors into log lines, so the test only needs the
    /// success path.
    struct StubStore;

    #[async_trait::async_trait]
    impl SessionStore for StubStore {
        async fn next_event_seq(&self, _agent_id: Uuid) -> Result<i64, StoreError> {
            Ok(1)
        }
        async fn insert_event(
            &self,
            _agent_id: Uuid,
            _seq: i64,
            _kind: &str,
            _payload_json: &str,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        async fn insert_event_value(
            &self,
            _agent_id: Uuid,
            _seq: i64,
            _kind: &str,
            _payload: serde_json::Value,
        ) -> Result<(), StoreError> {
            Ok(())
        }
        async fn list_events_since(
            &self,
            _agent_id: Uuid,
            _since: i64,
            _limit: u64,
        ) -> Result<Vec<AgentEventRecord>, StoreError> {
            Ok(Vec::new())
        }
    }

    /// `WsTransport` backed by an `mpsc` channel so the test can drive
    /// `run_inner` deterministically. Mirrors the in-module mock in
    /// `transport::tests` but is local here because the actor's test
    /// mod doesn't (and shouldn't) reach across module boundaries for
    /// test fixtures.
    struct MockWsTransport {
        inbound: tokio::sync::mpsc::UnboundedReceiver<TransportMsg>,
        outbound: Arc<Mutex<Vec<TransportMsg>>>,
    }

    impl Stream for MockWsTransport {
        type Item = std::io::Result<TransportMsg>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            std::pin::Pin::new(&mut self.inbound)
                .poll_recv(cx)
                .map(|opt| opt.map(Ok))
        }
    }

    impl Sink<TransportMsg> for MockWsTransport {
        type Error = std::io::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(
            self: std::pin::Pin<&mut Self>,
            item: TransportMsg,
        ) -> Result<(), Self::Error> {
            self.outbound.lock().unwrap().push(item);
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    impl WsTransport for MockWsTransport {
        fn close_reason(&self) -> Option<CloseReason> {
            None
        }
    }

    /// Drives `run_inner` through a complete `Hello → Close` cycle and
    /// asserts the post-loop offline broadcast lands on a subscribed
    /// `meta_rx` as `State { state: AgentState::Dead, .. }`. This is
    /// the connection-lost surfacing behavior added in the recent
    /// fix: before it, `run_inner` fell off the end of the function
    /// with no broadcast and the UI kept showing whatever state the
    /// last `Hello` carried (a stale green badge for hours).
    #[tokio::test]
    async fn run_inner_broadcasts_dead_state_on_socket_close() {
        let agent_id = Uuid::new_v4();
        let handle = AgentHandle::new(agent_id);
        // Subscribe BEFORE the actor publishes, so we don't race
        // broadcast's laggy-receiver semantics (subscribers attached
        // after `send` miss the event).
        let mut meta_rx = handle.subscribe_meta();

        // The Hello envelope the actor expects on the first inbound
        // text frame. `read_hello` parses this and the actor uses
        // `hello.state` to seed the initial `update_state` broadcast.
        let hello = Envelope {
            v: PROTOCOL_VERSION,
            seq: 1,
            body: EnvelopeBody::Hello(HelloUp {
                agent_id,
                protocol_v: PROTOCOL_VERSION,
                claude_version: "test-1.0".into(),
                session_id: None,
                state: AgentState::Idle,
                term_size: TermSize { cols: 80, rows: 24 },
            }),
        };
        let hello_json = serde_json::to_string(&hello).expect("serialize hello");

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<TransportMsg>();
        tx.send(TransportMsg::Text(hello_json))
            .expect("preload hello");

        let transport = MockWsTransport {
            inbound: rx,
            outbound: Arc::new(Mutex::new(Vec::new())),
        };

        let store: Arc<dyn SessionStore> = Arc::new(StubStore);
        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(8);

        // Spawn `run_inner` and then drive the WS to a clean Close.
        // `cmd_tx` is dropped immediately so the cmd_rx arm returns
        // `None` too — both arms racing to break is fine; whichever
        // fires first drops us out of the loop.
        let join = tokio::spawn({
            let handle = handle.clone();
            async move {
                run_inner(store, handle, agent_id, transport, cmd_rx).await;
            }
        });
        drop(_cmd_tx);

        // Wait for the initial Idle broadcast — proves `run_inner`
        // reached the loop's select! and is alive.
        let initial = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match meta_rx.recv().await {
                    Ok(EnvelopeBody::State(s)) if s.state == AgentState::Idle => return s,
                    Ok(_) => continue,
                    Err(e) => panic!("meta channel closed unexpectedly: {e:?}"),
                }
            }
        })
        .await
        .expect("initial Idle state within 2s");
        assert_eq!(initial.state, AgentState::Idle);

        // Drive the actor's `TransportMsg::Close` arm — main loop
        // breaks, post-loop broadcast publishes State{Dead}.
        tx.send(TransportMsg::Close(None))
            .expect("send close to inbound");

        // Drain meta_rx until we see the Dead state. Anything else
        // (a trailing Idle from the initial subscribe, an Ack, etc.)
        // is fine; we only assert Dead was published.
        let offline = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match meta_rx.recv().await {
                    Ok(EnvelopeBody::State(s)) if s.state == AgentState::Dead => return s,
                    Ok(_) => continue,
                    Err(e) => panic!("meta channel closed unexpectedly: {e:?}"),
                }
            }
        })
        .await
        .expect("Dead state within 2s of Close");
        assert_eq!(offline.state, AgentState::Dead);
        assert!(
            offline.session_id.is_none(),
            "Dead broadcast clears session_id"
        );
        assert_eq!(
            offline.reason, None,
            "Dead broadcast carries no disconnect reason (warren cannot distinguish dead from backoff)"
        );

        // `run_inner` must return cleanly after the broadcast.
        tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("run_inner joins within 2s")
            .expect("join");
    }

    #[tokio::test]
    async fn claim_leader_broadcasts_leader_changed_to_subscribers() {
        let handle = AgentHandle::new(Uuid::nil());
        let mut meta_rx = handle.subscribe_meta();
        let id = Uuid::from_bytes([1; 16]);

        // Mimic the dispatch arm: claim + update_state + publish_meta.
        handle.claim_leader(id, 120, 40);
        handle.update_state(AgentStateSnapshot {
            term_size: Some(TermSize {
                cols: 120,
                rows: 40,
            }),
            ..AgentStateSnapshot::default()
        });
        handle.publish_meta(EnvelopeBody::LeaderChanged {
            leader_id: Some(id),
            cols: 120,
            rows: 40,
        });

        let mut saw_changed = false;
        for _ in 0..8 {
            match meta_rx.recv().await {
                Ok(EnvelopeBody::LeaderChanged {
                    leader_id,
                    cols,
                    rows,
                }) => {
                    assert_eq!(leader_id, Some(id));
                    assert_eq!(cols, 120);
                    assert_eq!(rows, 40);
                    saw_changed = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_changed, "LeaderChanged must have been broadcast");
    }

    #[tokio::test]
    async fn release_leader_broadcasts_with_none_id() {
        let handle = AgentHandle::new(Uuid::nil());
        let id = Uuid::from_bytes([2; 16]);
        handle.claim_leader(id, 100, 30);
        handle.update_state(AgentStateSnapshot {
            term_size: Some(TermSize {
                cols: 100,
                rows: 30,
            }),
            ..AgentStateSnapshot::default()
        });
        let mut meta_rx = handle.subscribe_meta();
        // Clear the prior broadcasts so we can observe the release cleanly.
        while meta_rx.try_recv().is_ok() {}

        // Mimic the dispatch arm: release + publish_meta.
        assert!(handle.release_leader(id));
        let cur = handle
            .snapshot()
            .term_size
            .unwrap_or(TermSize { cols: 0, rows: 0 });
        handle.publish_meta(EnvelopeBody::LeaderChanged {
            leader_id: None,
            cols: cur.cols,
            rows: cur.rows,
        });

        let got = meta_rx.recv().await.unwrap();
        match got {
            EnvelopeBody::LeaderChanged {
                leader_id,
                cols,
                rows,
            } => {
                assert_eq!(leader_id, None);
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
            }
            other => panic!("expected LeaderChanged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connection_closed_clears_leader_when_caller_was_leader() {
        let handle = AgentHandle::new(Uuid::nil());
        let id = Uuid::from_bytes([3; 16]);
        handle.claim_leader(id, 100, 30);
        assert_eq!(handle.current_leader(), Some((id, 100, 30)));
        // Mimic the dispatch arm for ConnectionClosed.
        let cleared = handle.clear_leader_if(id);
        assert!(cleared, "ConnectionClosed from leader must clear");
        assert_eq!(handle.current_leader(), None);
    }

    #[tokio::test]
    async fn connection_closed_no_op_when_caller_was_not_leader() {
        let handle = AgentHandle::new(Uuid::nil());
        let leader_id = Uuid::from_bytes([4; 16]);
        let other_id = Uuid::from_bytes([5; 16]);
        handle.claim_leader(leader_id, 100, 30);
        let cleared = handle.clear_leader_if(other_id);
        assert!(!cleared, "non-leader's ConnectionClosed must not clear");
        assert_eq!(handle.current_leader(), Some((leader_id, 100, 30)));
    }

    #[tokio::test]
    async fn resize_from_non_leader_dropped_at_handle_level() {
        // The actor's `Command::ResizeFromConnection` arm double-checks
        // `is_leader` before forwarding to rabbit. Mirror that gate here.
        let handle = AgentHandle::new(Uuid::nil());
        let leader_id = Uuid::from_bytes([6; 16]);
        let other_id = Uuid::from_bytes([7; 16]);
        handle.claim_leader(leader_id, 120, 40);
        assert!(handle.is_leader(leader_id));
        assert!(
            !handle.is_leader(other_id),
            "non-leader's resize would be dropped at the dispatch boundary"
        );
    }

    #[tokio::test]
    async fn send_keys_leader_gate_passes_for_leader() {
        let handle = AgentHandle::new(Uuid::nil());
        let leader_id = Uuid::from_bytes([10; 16]);
        handle.claim_leader(leader_id, 80, 24);
        let cid = leader_id;
        assert!(handle.is_leader(cid), "leader's SendKeys must reach rabbit");
    }

    #[tokio::test]
    async fn send_keys_leader_gate_drops_for_non_leader() {
        let handle = AgentHandle::new(Uuid::nil());
        let leader_id = Uuid::from_bytes([11; 16]);
        let follower_id = Uuid::from_bytes([12; 16]);
        handle.claim_leader(leader_id, 80, 24);
        assert!(
            !handle.is_leader(follower_id),
            "follower's SendKeys must be dropped at the dispatch boundary"
        );
    }

    #[tokio::test]
    async fn send_keys_leader_gate_bypassed_when_connection_id_is_none() {
        let handle = AgentHandle::new(Uuid::nil());
        assert_eq!(handle.current_leader(), None);
    }
}
