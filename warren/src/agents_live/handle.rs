use crate::agents_live::actor::{Command, TurnOutcomeMsg};
use crate::agents_live::wire::{AgentState, EnvelopeBody, StateFrame, TermSize, UsageSnapshot};
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
    /// Shared handle to the actor's command channel. Wrapped in
    /// `Arc<Mutex<Sender>>` (not a bare `mpsc::Sender`) so that
    /// `install_cmd_tx` — called by `ws_rabbit` when a fresh actor
    /// takes over after a reconnect — propagates to *every* clone of
    /// the handle, not just the registry entry that the install path
    /// happens to mutate. Without this, a browser WS that opened
    /// before the rabbit connected would keep sending commands into a
    /// disconnected (no-receiver) mpsc, and every action button click
    /// on that tab would 500 until the tab reconnected.
    cmd_tx: Arc<Mutex<mpsc::Sender<Command>>>,
    /// §D Milestone 5: recorder base URL advertised by rabbit in its Hello
    /// envelope. `None` when recording is disabled or rabbit hasn't
    /// (re)connected with a fresh Hello yet.
    recorder_url: Arc<Mutex<Option<String>>>,
    /// §A.6 leader-based resize: identity of the browser tab whose size
    /// drives the kernel PTY. None = no leader (every browser is a follower;
    /// no one's resize reaches rabbit). Stored in a separate mutex so the
    /// hot path on `Resize` from a browser doesn't take the full state lock.
    leader: Arc<Mutex<Option<LeaderInfo>>>,
}

/// §A.6 leader-based resize: per-tab identity of the leader + the size they
/// claimed. Cheap to clone (Uuid + 2 × u16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeaderInfo {
    connection_id: Uuid,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Clone)]
pub struct AgentStateSnapshot {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub claude_version: Option<String>,
    pub last_usage: UsageSnapshot,
    /// §D Milestone 5: latest recorder URL we know about for this agent.
    /// None = recorder disabled, recording not advertised by rabbit, or no
    /// Hello received yet. The history page checks this before linking to
    /// `recorder_url` — showing the page when the URL is unknown would
    /// 404 every click.
    pub recorder_url: Option<String>,
    /// §A.6 leader-based resize: most recent PTY size advertised by rabbit
    /// (config-time at startup, then refreshed via subsequent leader-driven
    /// `Resize` envelopes through the rabbit link). None until the first
    /// Hello arrives; updated whenever the actor sees a new size. The
    /// release-leader / disconnect-leader broadcast uses this as the
    /// `(cols, rows)` to attach to `LeaderChanged { leader_id: None }` so
    /// followers don't see "no leader + 0×0" when the field hasn't been
    /// populated yet.
    pub term_size: Option<TermSize>,
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
            recorder_url: None,
            term_size: None,
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
            cmd_tx: Arc::new(Mutex::new(cmd_tx)),
            recorder_url: Arc::new(Mutex::new(None)),
            leader: Arc::new(Mutex::new(None)),
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
            cmd_tx: Arc::new(Mutex::new(cmd_tx)),
            recorder_url: Arc::new(Mutex::new(None)),
            leader: Arc::new(Mutex::new(None)),
        }
    }

    /// Install a fresh command sender so all clones of this handle start
    /// dispatching to the new channel. Used by `ws_rabbit` after a fresh
    /// actor starts (post-rabbit-reconnect). The shared `Arc<Mutex<>>`
    /// indirection means the registry entry AND every other clone of
    /// the handle (a browser WS that cloned it before reconnect, an
    /// HTTP action handler that cached a clone, etc.) all see the
    /// replacement on their next `cmd_tx()` call.
    pub fn install_cmd_tx(&self, cmd_tx: mpsc::Sender<Command>) {
        *self.cmd_tx.lock().expect("cmd_tx poisoned") = cmd_tx;
    }

    /// Returns a clone of the currently-installed command sender. The
    /// sender is cheap to clone (it shares an inner `Arc` with all
    /// other clones), but we still take the brief `Mutex` lock to
    /// ensure the returned sender matches the live install — a
    /// concurrent `install_cmd_tx` can swap in a new sender at any
    /// time, and we want callers to send on the freshest one.
    pub fn cmd_tx(&self) -> mpsc::Sender<Command> {
        self.cmd_tx
            .lock()
            .expect("cmd_tx poisoned")
            .clone()
    }

    pub fn split_for_actor(self) -> (AgentHandle, mpsc::Sender<Command>, mpsc::Receiver<Command>) {
        // Create a fresh channel for the new actor. The returned handle
        // shares the SAME `Arc<Mutex<Sender>>` as the input self — the
        // outer Arc is cloned, not moved, so all clones of the handle
        // (including ones a browser WS made before this rabbit arrived)
        // see the inner sender swap when we update it below. Without
        // the explicit share, `..self` would `move` the inner `Arc`,
        // leaving other clones with a now-frozen sender.
        let (tx, rx) = mpsc::channel(64);
        let cmd_arc = self.cmd_tx.clone();
        let handle = AgentHandle {
            cmd_tx: cmd_arc,
            ..self
        };
        // Install the fresh sender into the shared slot. Equivalent to
        // `handle.install_cmd_tx(tx.clone())` — done inline to keep the
        // function self-contained.
        *handle.cmd_tx.lock().expect("cmd_tx poisoned") = tx.clone();
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
        let recorder_url = new_state.recorder_url.clone();
        let term_size = new_state.term_size;
        {
            let mut g = self.state.lock().expect("state mutex poisoned");
            g.state = state;
            g.session_id = session_id.clone();
            g.claude_version = claude_version;
            if has_usage {
                g.last_usage = usage;
            }
            // Update recorder_url whenever the snapshot carries one. Keeps
            // the latest advertised URL after a Hello refresh, and stays
            // sticky when subsequent snapshots omit the field (a state
            // update from rabbit shouldn't blank the URL).
            if recorder_url.is_some() {
                g.recorder_url = recorder_url;
            }
            // §A.6 leader-based resize: refresh term_size whenever the
            // snapshot carries one. Sticky when omitted, same convention as
            // recorder_url — a state change without a fresh term_size
            // shouldn't blank the cached size.
            if term_size.is_some() {
                g.term_size = term_size;
            }
        }
        let _ = self.meta_tx.send(EnvelopeBody::State(StateFrame {
            state,
            session_id,
            reason: None,
        }));
    }

    /// §D Milestone 5: stash the recorder URL advertised by rabbit in its
    /// Hello envelope. Called by the actor once per (re)connect. Independent
    /// of `update_state` so callers can update the URL without disturbing
    /// the snapshot fields. Returns `true` iff the URL actually changed
    /// (caller can use this to decide whether to push a meta broadcast).
    pub fn set_recorder_url(&self, url: Option<String>) -> bool {
        let mut g = self.recorder_url.lock().expect("recorder_url poisoned");
        if *g != url {
            *g = url;
            true
        } else {
            false
        }
    }

    /// §D Milestone 5: read the most recently advertised recorder URL, if
    /// any. `None` when recording is disabled or rabbit hasn't (re)Hello'd
    /// since startup. Callers (the history page) must gate on this rather
    /// than fabricating a default — otherwise dead-link 404s.
    pub fn recorder_url(&self) -> Option<String> {
        self.recorder_url.lock().expect("recorder_url poisoned").clone()
    }

    // §A.6 leader-based resize -------------------------------------------
    //
    // The handle exposes the leader state as plain accessors. The actor
    // wraps them with the broadcast / rabbit-resize side-effects. All
    // operations take a single short mutex lock; nothing here blocks on I/O.

    /// Claims leadership for `connection_id` at `(cols, rows)`. **Always
    /// succeeds** — if a leader is already set (and not disconnected), this
    /// overwrites it (transfers leadership). Returns `true` iff there was a
    /// prior leader (i.e. this is a transfer), `false` on initial claim.
    /// The bool is informational only; the actor still broadcasts
    /// `LeaderChanged` in both cases. There is no "claim rejected" path —
    /// the spec design is "manual claim always wins."
    pub fn claim_leader(&self, connection_id: Uuid, cols: u16, rows: u16) -> bool {
        let mut g = self.leader.lock().expect("leader poisoned");
        let was_prior = g.is_some();
        *g = Some(LeaderInfo {
            connection_id,
            cols,
            rows,
        });
        was_prior
    }

    /// Releases leadership if `connection_id` is the current leader. No-op
    /// if a different connection (or no one) holds leadership. Returns
    /// `true` iff the leader was actually cleared (so the actor can decide
    /// whether to broadcast `LeaderChanged { None }`).
    pub fn release_leader(&self, connection_id: Uuid) -> bool {
        let mut g = self.leader.lock().expect("leader poisoned");
        match *g {
            Some(info) if info.connection_id == connection_id => {
                *g = None;
                true
            }
            _ => false,
        }
    }

    /// Clears leadership if `connection_id` is the current leader. Called on
    /// browser WS teardown. Returns `true` iff a clear happened (mirrors
    /// `release_leader`'s contract).
    pub fn clear_leader_if(&self, connection_id: Uuid) -> bool {
        self.release_leader(connection_id)
    }

    /// Current leader as `(connection_id, cols, rows)`, or `None`.
    /// Used by the actor to populate the broadcast `LeaderChanged` envelope.
    pub fn current_leader(&self) -> Option<(Uuid, u16, u16)> {
        self.leader
            .lock()
            .expect("leader poisoned")
            .map(|i| (i.connection_id, i.cols, i.rows))
    }

    /// True iff `connection_id` is the current leader. The actor's resize
    /// filter uses this to drop non-leader `Resize` frames at the command
    /// boundary.
    pub fn is_leader(&self, connection_id: Uuid) -> bool {
        self.leader
            .lock()
            .expect("leader poisoned")
            .map(|i| i.connection_id == connection_id)
            .unwrap_or(false)
    }

    /// Update the leader's `(cols, rows)` in-place without changing the
    /// `connection_id`. Used when a leader's own Resize envelope reports a
    /// new size — the actor must refresh the cached size so subsequent
    /// `LeaderChanged` broadcasts reflect the current grid, not the
    /// claim-time grid.
    pub fn update_leader_size(&self, connection_id: Uuid, cols: u16, rows: u16) -> bool {
        let mut g = self.leader.lock().expect("leader poisoned");
        if let Some(info) = g.as_mut() {
            if info.connection_id == connection_id {
                info.cols = cols;
                info.rows = rows;
                return true;
            }
        }
        false
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
        if self.cmd_tx().send(cmd).await.is_err() {
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
        self.cmd_tx()
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
        self.cmd_tx()
            .send(Command::Compact)
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))
    }

    pub async fn interrupt(&self) -> AppResult<()> {
        self.cmd_tx()
            .send(Command::Interrupt)
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))
    }

    pub async fn restart(&self, fresh: bool) -> AppResult<()> {
        self.cmd_tx()
            .send(Command::Restart { fresh })
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))
    }

    /// Send raw terminal bytes toward rabbit on the given channel
    /// (`TERM_CHAN_CLAUDE` or `TERM_CHAN_SHELL`). The channel decides which
    /// PTY on the rabbit side receives the keystrokes.
    pub async fn send_terminal_bytes(&self, chan: u8, bytes: Bytes) -> AppResult<()> {
        self.cmd_tx()
            .send(Command::SendKeys { chan, data: bytes })
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))?;
        Ok(())
    }

    /// Ask rabbit to force a full TUI redraw by emitting two SIGWINCHs.
    /// Called by the browser WS join path after the bounded replay buffer
    /// has been pushed into a fresh xterm.js pane.
    pub async fn repaint(&self) -> AppResult<()> {
        self.cmd_tx()
            .send(Command::Repaint)
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))?;
        Ok(())
    }

    /// §D Milestone 5 (Phase B): ask rabbit to emit a `ScreenSnapshot`
    /// envelope for the given channel. Called by the browser WS right after
    /// flushing the bounded replay buffer; the resulting snapshot lets the
    /// browser paint an authoritative terminal state, replacing the v1
    /// SIGWINCH-jiggle heuristic.
    pub async fn snapshot_request(&self, chan: u8) -> AppResult<()> {
        self.cmd_tx()
            .send(Command::SnapshotRequest { chan })
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))?;
        Ok(())
    }

    /// Route a browser-driven terminal resize through rabbit over the wire
    /// (`EnvelopeBody::Resize` → `PtyCmd::Resize` → `ioctl(TIOCSWINSZ)` +
    /// SIGWINCH), instead of typing a private xterm escape sequence into
    /// claude's PTY. The actor's `Command::Resize { cols, rows }` variant
    /// already exists from the late-join jiggle flow.
    pub async fn resize(&self, cols: u16, rows: u16) -> AppResult<()> {
        self.cmd_tx()
            .send(Command::Resize { cols, rows })
            .await
            .map_err(|_| AppError::Internal(anyhow::anyhow!("actor not running")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! §A.6 leader-based resize: state machine for the `leader` field.
    //!
    //! Each accessor is exercised across every meaningful transition. The
    //! most important assertion is the "transfer while connected" case —
    //! claim must succeed even when a prior leader is still around (the
    //! common path: a second tab takes control from the first).
    use super::*;

    fn h() -> AgentHandle {
        AgentHandle::new(Uuid::nil())
    }

    fn cid(byte: u8) -> Uuid {
        // Stable, distinct ids per test byte.
        Uuid::from_bytes([byte; 16])
    }

    /// Regression: `install_cmd_tx` must propagate the new sender to *every*
    /// clone of the handle, not just the registry entry that the install
    /// path happens to mutate. Before this was fixed, a browser WS that
    /// cloned the handle before a rabbit connected kept an orphaned sender
    /// (a `mpsc::Sender` whose receiver had never been wired) and every
    /// action-button click on that tab failed until the tab reloaded. The
    /// fix wraps `cmd_tx` in `Arc<Mutex<Sender>>` so `cmd_tx()` always
    /// returns the freshest installed sender.
    #[tokio::test]
    async fn install_cmd_tx_propagates_to_existing_clones() {
        let handle = h();
        // Take two clones BEFORE any install — they share the original
        // sender via the inner `Arc`.
        let stale_a = handle.clone();
        let stale_b = handle.clone();

        // Install a fresh sender with its own receiver. The actor will
        // take this sender and consume from `rx_post`.
        let (fresh_tx, mut rx_post) = tokio::sync::mpsc::channel::<Command>(8);
        handle.install_cmd_tx(fresh_tx);

        // Both stale clones must dispatch on the FRESH channel now.
        stale_a
            .cmd_tx()
            .send(Command::Compact)
            .await
            .expect("fresh channel is open");
        let cmd = rx_post
            .recv()
            .await
            .expect("fresh receiver got the command");
        assert!(matches!(cmd, Command::Compact));

        // And the second stale clone — proves the inner Arc is genuinely
        // shared, not just refreshed on one clone.
        stale_b
            .cmd_tx()
            .send(Command::Interrupt)
            .await
            .expect("fresh channel is open");
        let cmd = rx_post
            .recv()
            .await
            .expect("fresh receiver got the second command");
        assert!(matches!(cmd, Command::Interrupt));

        // Original handle still routes to fresh — sanity check that
        // `..self` in `split_for_actor` doesn't accidentally drop the
        // inner Arc.
        handle
            .cmd_tx()
            .send(Command::Compact)
            .await
            .expect("fresh channel is open");
        drop(stale_a);
        drop(stale_b);
        let _ = rx_post.recv().await;
    }

    /// Pure unit test of the same guarantee without channels: cloning a
    /// handle and calling `install_cmd_tx` on the original must change
    /// what `cmd_tx()` returns on the clones. (Tokio mpsc bridges this to
    /// the runtime; this is the synchronous form, useful for diagnosing
    /// any future regression where the lock stops being shared.)
    #[test]
    fn install_cmd_tx_changes_cmd_tx_return_on_clones() {
        let handle = h();
        let stale = handle.clone();

        // Capture the original sender identity via `Sender::same_channel`
        // (tokio 1.13+): two senders point to the same channel iff they
        // compare equal on `same_channel`. `AgentHandle::new` creates
        // an mpsc::channel(64) and drops the receiver immediately, so
        // the sender is "closed for sending" but still has its own
        // identity — we use that identity to detect the install.
        let original_sender = handle.cmd_tx();

        // Replace with a new sender.
        let (fresh_tx, _fresh_rx) = tokio::sync::mpsc::channel::<Command>(8);
        handle.install_cmd_tx(fresh_tx);

        // Stale clone must now route to the fresh sender — call
        // `cmd_tx()` on the stale clone and compare with the freshly-
        // installed sender (which we can recover from the original
        // handle via `cmd_tx()` again).
        let from_stale = stale.cmd_tx();
        let from_handle = handle.cmd_tx();
        assert!(
            from_stale.same_channel(&from_handle),
            "stale clone must share its sender slot with the original handle"
        );
        assert!(
            !from_stale.same_channel(&original_sender),
            "stale clone must NOT still point at the original sender"
        );
    }

    #[test]
    fn initial_state_has_no_leader() {
        let handle = h();
        assert_eq!(handle.current_leader(), None);
        assert!(!handle.is_leader(cid(1)));
    }

    #[test]
    fn update_state_term_size_sticky_when_omitted() {
        // §A.6 leader-based resize: a state update from rabbit that omits
        // term_size must not blank the cached size. The release-leader /
        // disconnect-leader broadcast reads from the cached term_size, so
        // blanking it would emit `LeaderChanged { leader_id: None, 0, 0 }`
        // — visibly wrong.
        let handle = h();
        let initial = TermSize { cols: 120, rows: 40 };
        handle.update_state(AgentStateSnapshot {
            term_size: Some(initial),
            ..AgentStateSnapshot::default()
        });
        assert_eq!(handle.snapshot().term_size, Some(initial));

        // Refresh without term_size — should keep the prior value.
        handle.update_state(AgentStateSnapshot {
            state: crate::agents_live::wire::AgentState::Idle,
            ..AgentStateSnapshot::default()
        });
        assert_eq!(
            handle.snapshot().term_size,
            Some(initial),
            "term_size must be sticky when omitted from a snapshot"
        );
    }

    #[test]
    fn update_state_term_size_refreshed_when_provided() {
        let handle = h();
        handle.update_state(AgentStateSnapshot {
            term_size: Some(TermSize { cols: 120, rows: 40 }),
            ..AgentStateSnapshot::default()
        });
        handle.update_state(AgentStateSnapshot {
            term_size: Some(TermSize { cols: 80, rows: 24 }),
            ..AgentStateSnapshot::default()
        });
        assert_eq!(
            handle.snapshot().term_size,
            Some(TermSize { cols: 80, rows: 24 }),
            "term_size must update when a fresh value is provided"
        );
    }

    #[test]
    fn claim_leader_with_no_prior_leader_sets_it_and_returns_false() {
        let handle = h();
        let was_prior = handle.claim_leader(cid(1), 120, 40);
        assert!(!was_prior, "first claim must report no prior");
        assert_eq!(handle.current_leader(), Some((cid(1), 120, 40)));
        assert!(handle.is_leader(cid(1)));
        assert!(!handle.is_leader(cid(2)));
    }

    #[test]
    fn second_claim_transfers_leadership_even_when_first_leader_is_connected() {
        // The "operator opens a second tab to take control" path. Prior
        // leader is NOT disconnected; claim still succeeds and returns
        // true (was_prior = true).
        let handle = h();
        let _ = handle.claim_leader(cid(1), 120, 40);
        let was_prior = handle.claim_leader(cid(2), 80, 24);
        assert!(was_prior, "second claim while connected must report prior");
        assert_eq!(handle.current_leader(), Some((cid(2), 80, 24)));
        assert!(!handle.is_leader(cid(1)));
        assert!(handle.is_leader(cid(2)));
    }

    #[test]
    fn release_leader_clears_when_caller_is_leader() {
        let handle = h();
        handle.claim_leader(cid(1), 120, 40);
        assert!(handle.release_leader(cid(1)));
        assert_eq!(handle.current_leader(), None);
    }

    #[test]
    fn release_leader_no_op_when_caller_is_not_leader() {
        let handle = h();
        handle.claim_leader(cid(1), 120, 40);
        // cid(2) is not the leader; their release must be a no-op.
        assert!(!handle.release_leader(cid(2)));
        // Leader must still be cid(1).
        assert_eq!(handle.current_leader(), Some((cid(1), 120, 40)));
    }

    #[test]
    fn release_leader_no_op_when_no_leader() {
        let handle = h();
        assert!(!handle.release_leader(cid(1)));
    }

    #[test]
    fn clear_leader_if_only_clears_for_matching_id() {
        let handle = h();
        handle.claim_leader(cid(1), 120, 40);
        assert!(handle.clear_leader_if(cid(1)));
        assert_eq!(handle.current_leader(), None);

        // Re-claim and clear with the wrong id — should not affect state.
        handle.claim_leader(cid(1), 120, 40);
        assert!(!handle.clear_leader_if(cid(2)));
        assert_eq!(handle.current_leader(), Some((cid(1), 120, 40)));
    }

    #[test]
    fn update_leader_size_only_updates_when_caller_is_leader() {
        let handle = h();
        handle.claim_leader(cid(1), 120, 40);
        // Leader updates their own size — works.
        assert!(handle.update_leader_size(cid(1), 100, 30));
        assert_eq!(handle.current_leader(), Some((cid(1), 100, 30)));
        // Non-leader's update — no-op, leader untouched.
        assert!(!handle.update_leader_size(cid(2), 50, 20));
        assert_eq!(handle.current_leader(), Some((cid(1), 100, 30)));
    }

    #[test]
    fn full_state_machine_walk() {
        // Exhaustive table-driven walk of every meaningful transition.
        let handle = h();
        let a = cid(1);
        let b = cid(2);

        // 1. Initial → no leader
        assert_eq!(handle.current_leader(), None);
        assert!(!handle.is_leader(a));
        assert!(!handle.is_leader(b));

        // 2. A claims (initial)
        assert!(!handle.claim_leader(a, 120, 40));
        assert!(handle.is_leader(a));
        assert!(!handle.is_leader(b));

        // 3. B's release is a no-op
        assert!(!handle.release_leader(b));
        assert!(handle.is_leader(a));

        // 4. A updates size
        assert!(handle.update_leader_size(a, 100, 30));
        assert_eq!(handle.current_leader(), Some((a, 100, 30)));

        // 5. B claims — transfer
        assert!(handle.claim_leader(b, 80, 24));
        assert!(!handle.is_leader(a));
        assert!(handle.is_leader(b));
        assert_eq!(handle.current_leader(), Some((b, 80, 24)));

        // 6. A's release is a no-op now
        assert!(!handle.release_leader(a));
        assert!(handle.is_leader(b));

        // 7. A's clear is a no-op
        assert!(!handle.clear_leader_if(a));
        assert!(handle.is_leader(b));

        // 8. B disconnects (clear_leader_if with B's id)
        assert!(handle.clear_leader_if(b));
        assert_eq!(handle.current_leader(), None);

        // 9. A's release post-disconnect is a no-op
        assert!(!handle.release_leader(a));
    }
}
