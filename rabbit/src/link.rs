use crate::wire::{Envelope, EnvelopeBody, HelloUp, TermSize, PROTOCOL_VERSION, TERM_CHAN_CLAUDE};
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
    /// Raw PTY bytes forwarded from warren (channel byte already stripped).
    /// Only the Claude channel (0x01) is forwarded; other channel ids are dropped.
    Binary(Vec<u8>),
    Text(Envelope),
}

pub enum LinkCmd {
    SendBinary(Vec<u8>),
    #[allow(dead_code)]
    SendText(Envelope),
    SendTextRaw(String),
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
        let base = self
            .warren_url
            .trim_end_matches('/');
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

        loop {
            tokio::select! {
                biased;
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break; };
                    match cmd {
                        LinkCmd::SendBinary(mut b) => {
                            if b.is_empty() { continue; }
                            let mut frame = vec![TERM_CHAN_CLAUDE];
                            frame.append(&mut b);
                            sink.send(Message::Binary(frame)).await?;
                        }
                        LinkCmd::SendText(env) => {
                            let s = serde_json::to_string(&env)?;
                            sink.send(Message::Text(s)).await?;
                        }
                        LinkCmd::SendTextRaw(s) => {
                            sink.send(Message::Text(s)).await?;
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
                                let _ = self.event_tx.send(LinkEvent::Text(env)).await;
                            }
                        }
                        Message::Binary(mut b) => {
                            // warren frames every binary with a leading channel byte.
                            // Currently only the Claude channel is defined; drop
                            // anything else rather than feeding it to the PTY.
                            if b.is_empty() {
                                continue;
                            }
                            let chan = b.remove(0);
                            if chan == TERM_CHAN_CLAUDE {
                                let _ = self.event_tx.send(LinkEvent::Binary(b)).await;
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
