use crate::agents_live::actor::{self, Command, TurnOutcomeMsg};
use crate::agents_live::wire::{AgentState, EnvelopeBody, StateFrame, UsageSnapshot};
use crate::error::{AppError, AppResult};
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

/// Upper bound on how many recent terminal chunks we hold for late browser
/// WS joiners. The actor feeds up-to-4096-byte chunks; 128 chunks ≈ 512 KB
/// per agent, which is well under any realistic screen budget.
const TERM_RING_MAX_CHUNKS: usize = 128;

#[derive(Clone)]
pub struct AgentHandle {
    pub agent_id: Uuid,
    state: Arc<Mutex<AgentStateSnapshot>>,
    term_tx: broadcast::Sender<Bytes>,
    /// Bounded ring of the most recent terminal bytes for late subscribers.
    /// `broadcast::Sender` only delivers to receivers attached at send time,
    /// so a browser WS that joins after the agent has been running would
    /// otherwise see an empty screen. This ring fills the gap.
    term_ring: Arc<Mutex<VecDeque<Bytes>>>,
    meta_tx: broadcast::Sender<EnvelopeBody>,
    cmd_tx: mpsc::Sender<Command>,
}

#[derive(Debug, Clone)]
pub struct AgentStateSnapshot {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub claude_version: Option<String>,
    pub last_usage: UsageSnapshot,
}

impl Default for AgentStateSnapshot {
    fn default() -> Self {
        Self {
            state: AgentState::Starting,
            session_id: None,
            claude_version: None,
            last_usage: UsageSnapshot {
                source: "transcript".to_string(),
                ..Default::default()
            },
        }
    }
}

impl AgentHandle {
    pub fn new(agent_id: Uuid) -> Self {
        let (term_tx, _) = broadcast::channel(1024);
        let (meta_tx, _) = broadcast::channel(1024);
        let (cmd_tx, _) = mpsc::channel(64);
        Self {
            agent_id,
            state: Arc::new(Mutex::new(AgentStateSnapshot::default())),
            term_tx,
            term_ring: Arc::new(Mutex::new(VecDeque::with_capacity(TERM_RING_MAX_CHUNKS))),
            meta_tx,
            cmd_tx,
        }
    }

    pub fn with_cmd_tx(agent_id: Uuid, cmd_tx: mpsc::Sender<Command>) -> Self {
        let (term_tx, _) = broadcast::channel(1024);
        let (meta_tx, _) = broadcast::channel(1024);
        Self {
            agent_id,
            state: Arc::new(Mutex::new(AgentStateSnapshot::default())),
            term_tx,
            term_ring: Arc::new(Mutex::new(VecDeque::with_capacity(TERM_RING_MAX_CHUNKS))),
            meta_tx,
            cmd_tx,
        }
    }

    pub fn install_cmd_tx(&mut self, cmd_tx: mpsc::Sender<Command>) {
        self.cmd_tx = cmd_tx;
    }

    pub fn split_for_actor(self) -> (AgentHandle, mpsc::Sender<Command>, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel(64);
        let handle = AgentHandle {
            cmd_tx: tx.clone(),
            ..self
        };
        (handle, tx, rx)
    }

    pub fn snapshot(&self) -> AgentStateSnapshot {
        self.state.lock().expect("state mutex poisoned").clone()
    }

    pub fn update_state(&self, new_state: AgentStateSnapshot) {
        let state = new_state.state;
        let session_id = new_state.session_id.clone();
        let claude_version = new_state.claude_version.clone();
        let usage = new_state.last_usage.clone();
        let has_usage =
            new_state.last_usage.input_tokens > 0 || new_state.last_usage.output_tokens > 0;
        {
            let mut g = self.state.lock().expect("state mutex poisoned");
            g.state = state;
            g.session_id = session_id.clone();
            g.claude_version = claude_version;
            if has_usage {
                g.last_usage = usage;
            }
        }
        let _ = self.meta_tx.send(EnvelopeBody::State(StateFrame {
            state,
            session_id,
            reason: None,
        }));
    }

    pub fn publish_term(&self, bytes: Bytes) {
        // Push into the ring first (bounded) so a slow subscriber that joins
        // later can still replay the recent screen. Broadcast is best-effort;
        // only currently-attached receivers see it live.
        {
            let mut ring = self.term_ring.lock().expect("term_ring poisoned");
            if ring.len() == TERM_RING_MAX_CHUNKS {
                ring.pop_front();
            }
            ring.push_back(bytes.clone());
        }
        let _ = self.term_tx.send(bytes);
    }

    pub fn publish_meta(&self, ev: EnvelopeBody) {
        let _ = self.meta_tx.send(ev);
    }

    pub fn subscribe_term(&self) -> broadcast::Receiver<Bytes> {
        self.term_tx.subscribe()
    }

    pub fn subscribe_meta(&self) -> broadcast::Receiver<EnvelopeBody> {
        self.meta_tx.subscribe()
    }

    pub fn replay_term(&self) -> Vec<Bytes> {
        let ring = self.term_ring.lock().expect("term_ring poisoned");
        ring.iter().cloned().collect()
    }

    pub async fn prompt(&self, text: &str, wait: bool) -> AppResult<TurnOutcomeMsg> {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let cmd = Command::Prompt {
            id,
            text: text.to_string(),
            by: "admin".to_string(),
            wait,
            reply: if wait { Some(tx) } else { None },
        };
        if self.cmd_tx.send(cmd).await.is_err() {
            return Err(AppError::Internal(anyhow::anyhow!(
                "agent actor not running"
            )));
        }
        if wait {
            rx.await
                .map_err(|_| AppError::Internal(anyhow::anyhow!("actor dropped")))
        } else {
            let now = chrono::Utc::now();
            Ok(TurnOutcomeMsg {
                prompt_id: id,
                started_at: now,
                ended_at: now,
                usage: None,
                error: None,
            })
        }
    }

    pub async fn usage(&self) -> AppResult<UsageSnapshot> {
        Ok(self.snapshot().last_usage)
    }

    pub async fn state(&self) -> AppResult<AgentStateSnapshot> {
        Ok(self.snapshot())
    }

    pub async fn clear(&self, hard: bool) -> AppResult<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Clear {
                hard,
                reply: Some(tx),
            })
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))?;
        rx.await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor dropped")))
    }

    pub async fn compact(&self) -> AppResult<()> {
        self.cmd_tx
            .send(Command::Compact)
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))
    }

    pub async fn interrupt(&self) -> AppResult<()> {
        self.cmd_tx
            .send(Command::Interrupt)
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))
    }

    pub async fn restart(&self, fresh: bool) -> AppResult<()> {
        self.cmd_tx
            .send(Command::Restart { fresh })
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))
    }

    pub async fn send_terminal_bytes(&self, bytes: Bytes) -> AppResult<()> {
        self.cmd_tx
            .send(Command::SendKeys(bytes))
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))?;
        Ok(())
    }
}

#[allow(dead_code)]
pub fn _actor_link() -> actor::ActorHandle {
    let (tx, _rx) = mpsc::channel(1);
    let join = tokio::spawn(async move {});
    actor::ActorHandle { cmd_tx: tx, join }
}
