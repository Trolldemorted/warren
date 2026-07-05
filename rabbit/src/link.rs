use crate::meta_ring::MetaRing;
use crate::wire::{
    Envelope, EnvelopeBody, HelloUp, TermSize, PROTOCOL_VERSION, TERM_CHAN_CLAUDE, TERM_CHAN_SHELL,
};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicI64, Ordering};
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
    /// Raw PTY bytes from a terminal → warren → viewers, tagged with the
    /// channel byte the frame should carry on the wire (claude vs. shell).
    SendBinary { chan: u8, data: Vec<u8> },
    /// Structured meta event. The link assigns the next seq, buffers the
    /// serialized frame in the meta ring, then sends. The frame is replayed
    /// on the next WS attempt until warren sends `Ack{seq}` for it.
    SendMeta(EnvelopeBody),
    #[allow(dead_code)]
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
}

/// Returns the current replay bytes (concatenated, in order) to send as the
/// initial binary frame on each link attempt. Empty Vec = no replay.
pub type ReplaySnapFn = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

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
        }
    }

    #[allow(dead_code)]
    pub fn seq_handle(&self) -> Arc<AtomicI64> {
        self.seq.clone()
    }

    pub async fn run(mut self) -> Result<()> {
        let mut backoff = Duration::from_millis(250);
        loop {
            match self.attempt().await {
                Ok(()) => {
                    log::info!("warren link closed cleanly");
                    backoff = Duration::from_millis(250);
                }
                Err(e) => {
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
                state: "starting".to_string(),
                term_size: self.term_size,
                recorder_url: self.recorder_url.clone(),
            }),
        };
        let hello_json = serde_json::to_string(&hello)?;
        let (mut sink, mut stream) = ws.split();
        sink.send(Message::Text(hello_json)).await?;
        let replay_bytes = (self.replay_snap)();
        if !replay_bytes.is_empty() {
            let mut frame = vec![TERM_CHAN_CLAUDE];
            frame.extend_from_slice(&replay_bytes);
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
                        LinkCmd::SendBinary { chan, mut data } => {
                            if data.is_empty() { continue; }
                            let mut frame = vec![chan];
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
