use crate::meta_ring::MetaRing;
use crate::wire::{
    Envelope, EnvelopeBody, HelloUp, TermFrame, TermSize, PROTOCOL_VERSION, TERM_CHAN_CLAUDE,
    TERM_CHAN_SHELL, AgentState,
};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub enum LinkEvent {
    /// Raw PTY bytes forwarded from warren, tagged with the terminal channel
    /// they belong to (`TERM_CHAN_CLAUDE` or `TERM_CHAN_SHELL`). The supervisor
    /// routes each frame to the matching PTY. Unknown channel ids are dropped
    /// in `attempt` before they ever reach here.
    Binary { chan: u8, data: Vec<u8> },
    Text(Envelope),
}

pub enum LinkCmd {
    /// §A.7 / seq-numbered snapshot protocol — raw PTY bytes from a
    /// terminal → warren → viewers, tagged with the channel byte and a
    /// per-channel monotonic `seq` (the same value the blocking PTY
    /// thread assigned when the bytes were read). The link prepends
    /// `<chan:1> <seq:8 BE>` to the frame on the wire; warren passes
    /// both through verbatim and forwards them to the browser, so the
    /// browser can pin a late-arriving `ScreenSnapshot::after_seq`
    /// against its buffered live frames.
    SendBinary { chan: u8, seq: u64, data: Vec<u8> },
    /// Structured meta event. The link assigns the next seq, buffers the
    /// serialized frame in the meta ring, then sends. The frame is replayed
    /// on the next WS attempt until warren sends `Ack{seq}` for it.
    SendMeta(EnvelopeBody),
    /// Sent by the supervisor just before its outer loop exits so the link's
    /// `attempt()` issues `Message::Close` to warren and returns cleanly. The
    /// `Arc<AtomicBool>` `shutdown` flag is the backstop that breaks the
    /// reconnect loop even when this never reaches the link.
    Shutdown,
}

pub struct Link {
    warren_url: String,
    agent_token: String,
    agent_id: uuid::Uuid,
    claude_version: String,
    seq: Arc<AtomicI64>,
    term_size: TermSize,
    cmd_rx: mpsc::Receiver<LinkCmd>,
    event_tx: mpsc::Sender<LinkEvent>,
    /// Called at the start of each WS attempt to fetch the latest screen
    /// snapshot for the rabbit→warren replay frame. Captured once at link
    /// construction (cheap Arc clone); queried per reconnect so a rabbit
    /// that drops and reconnects sends the current state, not a stale one.
    replay_snap: ReplaySnapFn,
    /// Bounded queue of recently-sent meta events awaiting Ack. Survives
    /// across WS attempts within a single Link lifetime.
    meta_ring: Arc<MetaRing>,
    /// §D Milestone 5: absolute base URL of this rabbit's recorder HTTP
    /// server, advertised in the Hello envelope so warren can fetch `.cast`
    /// files for `/agent/:id/claude/history`. `None` when recording is
    /// disabled.
    recorder_url: Option<String>,
    /// Supervisor-shared shutdown flag. When `true`, `run()` exits its
    /// reconnect loop instead of bouncing forever — without this, a clean
    /// supervisor shutdown still leaves a forever-retriying link task that
    /// holds the runtime open. The supervisor also pushes
    /// `LinkCmd::Shutdown` for a graceful WS close; this flag is the
    /// backstop that breaks the outer reconnect loop no matter how
    /// `attempt()` returned.
    shutdown: Arc<AtomicBool>,
}

/// Returns the current replay frames (in chronological order) to send as the
/// initial sequence of binary frames on each link attempt. One binary frame
/// per element; each frame carries `<chan:1> <seq:8 BE> <data>` on the wire.
/// Empty Vec = no replay.
pub type ReplaySnapFn = Arc<dyn Fn() -> Vec<TermFrame> + Send + Sync>;

impl Link {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        warren_url: String,
        agent_token: String,
        agent_id: uuid::Uuid,
        claude_version: String,
        term_size: TermSize,
        cmd_rx: mpsc::Receiver<LinkCmd>,
        event_tx: mpsc::Sender<LinkEvent>,
        replay_snap: ReplaySnapFn,
        meta_ring: Arc<MetaRing>,
        recorder_url: Option<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            warren_url,
            agent_token,
            agent_id,
            claude_version,
            seq: Arc::new(AtomicI64::new(1)),
            term_size,
            cmd_rx,
            event_tx,
            replay_snap,
            meta_ring,
            recorder_url,
            shutdown,
        }
    }

    #[allow(dead_code)]
    pub fn seq_handle(&self) -> Arc<AtomicI64> {
        self.seq.clone()
    }

    pub async fn run(mut self) -> Result<()> {
        let mut backoff = Duration::from_millis(250);
        loop {
            // Check shutdown before each attempt and after each return so a
            // successful connect+close doesn't trigger an immediate reconnect.
            // Without this the link task would live forever, holding the
            // tokio runtime open even after the supervisor's `run()` returns
            // — a graceful supervisor shutdown would still leave a forever
            // bouncing link task.
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("warren link shutting down");
                return Ok(());
            }
            match self.attempt().await {
                Ok(()) => {
                    log::info!("warren link closed cleanly");
                    backoff = Duration::from_millis(250);
                    if self.shutdown.load(Ordering::SeqCst) {
                        log::info!("warren link shutdown requested after clean close");
                        return Ok(());
                    }
                }
                Err(e) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        log::info!("warren link error during shutdown ({e:?}); exiting");
                        return Ok(());
                    }
                    log::warn!("warren link error: {e:?}; reconnecting in {backoff:?}");
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn attempt(&mut self) -> Result<()> {
        // The config accepts an http(s) URL because that's how users
        // naturally write it; tungstenite's WS client requires ws(s)://.
        // Rewrite the scheme at the use site rather than asking the operator
        // to remember the difference. The rabbit WS endpoint lives at
        // /ws/rabbit on warren; GET / would 303 to /admin/agents.
        let base = self.warren_url.trim_end_matches('/');
        let ws_url = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}/ws/rabbit")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}/ws/rabbit")
        } else {
            format!("{base}/ws/rabbit")
        };
        let mut req = ws_url
            .as_str()
            .into_client_request()
            .context("building warren request")?;
        req.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.agent_token).parse()?,
        );

        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .context("connecting to warren")?;
        log::info!("warren link up: {}", self.warren_url);

        let hello_seq = self.next_seq();
        let hello = Envelope {
            v: PROTOCOL_VERSION,
            seq: hello_seq,
            body: EnvelopeBody::Hello(HelloUp {
                agent_id: self.agent_id,
                protocol_v: PROTOCOL_VERSION,
                claude_version: self.claude_version.clone(),
                session_id: None,
                state: AgentState::Starting,
                term_size: self.term_size,
                recorder_url: self.recorder_url.clone(),
            }),
        };
        let hello_json = serde_json::to_string(&hello)?;
        let (mut sink, mut stream) = ws.split();
        sink.send(Message::Text(hello_json)).await?;
        // §A.7: each replay frame is its own `<chan:1> <seq:8 BE> <data>`
        // binary message, in the order the producer emitted them. warren
        // re-emits each frame verbatim to its browser subscribers so a
        // freshly-connected browser sees the exact same on-wire bytes
        // (including `seq`) that any other browser already subscribed
        // through this connection would have seen.
        let replay_frames = (self.replay_snap)();
        for TermFrame { chan, seq, data } in replay_frames {
            if data.is_empty() {
                continue;
            }
            let mut frame = Vec::with_capacity(9 + data.len());
            frame.push(chan);
            frame.extend_from_slice(&seq.to_be_bytes());
            frame.extend_from_slice(&data);
            sink.send(Message::Binary(frame)).await?;
        }
        // Replay any meta events warren hasn't acked yet. Seq numbers carry
        // over across reconnects (the AtomicI64 lives on the Link struct, not
        // the WS attempt), so warren's dedup-by-seq catches duplicates.
        for frame in self.meta_ring.snapshot() {
            sink.send(Message::Text(frame)).await?;
        }

        loop {
            tokio::select! {
                biased;
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break; };
                    match cmd {
                        // §A.7: every server→browser terminal binary
                        // frame is now `<chan:1> <seq:8 BE> <data>`.
                        // warren parses the prelude off the frame,
                        // forwards both `chan` and `seq` to its
                        // subscribers verbatim, and rewrites the same
                        // prelude when broadcasting to browser panes.
                        LinkCmd::SendBinary {
                            chan,
                            seq,
                            mut data,
                        } => {
                            if data.is_empty() { continue; }
                            let mut frame = Vec::with_capacity(9 + data.len());
                            frame.push(chan);
                            frame.extend_from_slice(&seq.to_be_bytes());
                            frame.append(&mut data);
                            sink.send(Message::Binary(frame)).await?;
                        }
                        LinkCmd::SendMeta(body) => {
                            let seq = self.next_seq();
                            let env = Envelope {
                                v: PROTOCOL_VERSION,
                                seq,
                                body,
                            };
                            let frame = serde_json::to_string(&env)?;
                            self.meta_ring.push(seq, frame.clone());
                            sink.send(Message::Text(frame)).await?;
                        }
                        LinkCmd::Shutdown => {
                            sink.send(Message::Close(None)).await.ok();
                            return Ok(());
                        }
                    }
                }
                msg = stream.next() => {
                    let Some(msg) = msg else { break; };
                    match msg? {
                        Message::Text(t) => {
                            if let Ok(env) = serde_json::from_str::<Envelope>(&t) {
                                if let EnvelopeBody::Ack { ack_seq } = env.body {
                                    let freed = self.meta_ring.trim_through(ack_seq);
                                    if freed > 0 {
                                        log::debug!(
                                            "warren acked through seq={ack_seq} (freed {freed} bytes of buffered meta)"
                                        );
                                    }
                                    continue;
                                }
                                let _ = self.event_tx.send(LinkEvent::Text(env)).await;
                            }
                        }
                        Message::Binary(mut b) => {
                            // warren frames every binary with a leading channel byte.
                            // Route the known terminal channels (claude + shell)
                            // through to the supervisor tagged with their channel;
                            // drop anything else rather than feeding it to a PTY.
                            if b.is_empty() {
                                continue;
                            }
                            let chan = b.remove(0);
                            if chan == TERM_CHAN_CLAUDE || chan == TERM_CHAN_SHELL {
                                let _ = self
                                    .event_tx
                                    .send(LinkEvent::Binary { chan, data: b })
                                    .await;
                            }
                        }
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) => {}
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    //! `Link::run` shutdown-exit contract. Without this, a graceful
    //! supervisor shutdown leaves the link task alive forever in its
    //! reconnect loop, which would prevent the binary from exiting on a
    //! clean `^C`.

    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    /// Build a `Link` pointing at `127.0.0.1:{port}`. The caller is
    /// responsible for actually listening on that port (or not). The
    /// returned shutdown flag is shared with the link so tests can flip
    /// it mid-flight.
    fn spawn_test_link(
        port: u16,
        shutdown: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let (_cmd_tx, cmd_rx) = mpsc::channel::<LinkCmd>(128);
        let (event_tx, _event_rx) = mpsc::channel::<LinkEvent>(128);
        let ring = Arc::new(MetaRing::new(262_144));
        let replay_snap: ReplaySnapFn = Arc::new(Vec::new);

        let link = Link::new(
            format!("http://127.0.0.1:{port}"),
            "test-token".into(),
            uuid::Uuid::nil(),
            "test-1.0".into(),
            TermSize { cols: 80, rows: 24 },
            cmd_rx,
            event_tx,
            replay_snap,
            ring,
            None,
            shutdown,
        );
        tokio::spawn(async move { link.run().await })
    }

    #[tokio::test]
    async fn run_exits_immediately_when_shutdown_set_before_start() {
        // Top-of-loop guard: if shutdown is already true when `run()` is
        // entered, the loop returns `Ok(())` before any WS attempt. We
        // point at an unreachable port; if the guard is broken, the loop
        // would bounce on connect errors and never finish in 2s.
        let shutdown = Arc::new(AtomicBool::new(true));
        let h = spawn_test_link(1, shutdown);
        let () = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("run must exit within 2s when shutdown is pre-set")
            .expect("join")
            .expect("Ok exit");
    }

    #[tokio::test]
    async fn run_exits_mid_backoff_when_shutdown_flipped() {
        // Err-path guard: connect fails fast (no listener), the Err arm
        // is about to enter its 250 ms sleep. We flip shutdown during
        // that window and assert run returns rather than sleeping through.
        let shutdown = Arc::new(AtomicBool::new(false));
        // Port 1 is reserved / unbindable on most platforms so connect()
        // fails immediately.
        let h = spawn_test_link(1, shutdown.clone());

        // Give the run loop time to attempt once and land in the backoff
        // sleep. 50 ms is generous on any reasonable host.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.store(true, Ordering::SeqCst);

        let () = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("run must exit within 2s after shutdown is flipped mid-flight")
            .expect("join")
            .expect("Ok exit");
    }
}
