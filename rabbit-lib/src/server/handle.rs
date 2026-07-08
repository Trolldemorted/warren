use crate::server::actor::{Command, TurnOutcomeMsg};
use crate::wire::{AgentState, EnvelopeBody, StateFrame, TermFrame, TermSize, UsageSnapshot};
use anyhow::Result as AnyResult;
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
    // §A.7: the broadcast/ring carry full `TermFrame`s (chan, seq, data)
    // so a browser pane that joins late can replay the seq alongside the
    // bytes — when a late-arriving `ScreenSnapshot::after_seq` arrives,
    // the browser trims its buffered frames to `seq > after_seq` before
    // applying the snapshot, which kills the empty-snapshot flicker the
    // old §A.6 SIGWINCH-jiggle heuristic patched around. Pre-v2 this was
    // `Bytes`; the upgrade widens the type shape (rabbit's wire still
    // discards the seq for browser→warren traffic — that's a separate
    // channel with no seq).
    term_tx: broadcast::Sender<TermFrame>,
    /// Bounded ring of the most recent terminal frames for late
    /// subscribers. `broadcast::Sender` only delivers to receivers
    /// attached at send time, so a browser WS that joins after the agent
    /// has been running would otherwise see an empty screen. This ring
    /// fills the gap.
    term_ring: Arc<Mutex<VecDeque<TermFrame>>>,
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
}

#[derive(Debug, Clone)]
pub struct AgentStateSnapshot {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub claude_version: Option<String>,
    pub last_usage: UsageSnapshot,
    /// §Simplify TUI sizing: most recent PTY size, populated from the
    /// `TuiConfig` envelope warren sends after the rabbit hello, and
    /// refreshed on subsequent `Command::Resize` dispatches. None until
    /// the first `TuiConfig` arrives.
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
        self.cmd_tx.lock().expect("cmd_tx poisoned").clone()
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
        let term_size = new_state.term_size;
        {
            let mut g = self.state.lock().expect("state mutex poisoned");
            g.state = state;
            g.session_id = session_id.clone();
            g.claude_version = claude_version;
            if has_usage {
                g.last_usage = usage;
            }
            // Refresh term_size whenever the snapshot carries one. Sticky when
            // omitted — a state change without a fresh term_size shouldn't
            // blank the cached size.
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

    pub fn publish_term(&self, frame: TermFrame) {
        // Push into the ring first (bounded) so a slow subscriber that joins
        // later can still replay the recent screen. Broadcast is best-effort;
        // only currently-attached receivers see it live.
        {
            let mut ring = self.term_ring.lock().expect("term_ring poisoned");
            if ring.len() == TERM_RING_MAX_CHUNKS {
                ring.pop_front();
            }
            ring.push_back(frame.clone());
        }
        let _ = self.term_tx.send(frame);
    }

    pub fn publish_meta(&self, ev: EnvelopeBody) {
        let _ = self.meta_tx.send(ev);
    }

    pub fn subscribe_term(&self) -> broadcast::Receiver<TermFrame> {
        self.term_tx.subscribe()
    }

    pub fn subscribe_meta(&self) -> broadcast::Receiver<EnvelopeBody> {
        self.meta_tx.subscribe()
    }

    pub fn replay_term(&self) -> Vec<TermFrame> {
        let ring = self.term_ring.lock().expect("term_ring poisoned");
        ring.iter().cloned().collect()
    }

    pub async fn prompt(&self, text: &str, wait: bool) -> AnyResult<TurnOutcomeMsg> {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let cmd = Command::Prompt {
            id,
            text: text.to_string(),
            by: "admin".to_string(),
            wait,
            reply: if wait { Some(tx) } else { None },
            by_connection_id: None,
        };
        if self.cmd_tx().send(cmd).await.is_err() {
            return Err(anyhow::anyhow!("agent actor not running"));
        }
        if wait {
            rx.await.map_err(|_| anyhow::anyhow!("actor dropped"))
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

    /// Prompt with a stamped `connection_id` so `PromptEcho` and
    /// `PromptRejected` broadcasts carry the originating tab's id.
    pub async fn prompt_with_origin(
        &self,
        text: &str,
        wait: bool,
        connection_id: Uuid,
    ) -> AnyResult<TurnOutcomeMsg> {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let cmd = Command::Prompt {
            id,
            text: text.to_string(),
            by: "browser".to_string(),
            wait,
            reply: if wait { Some(tx) } else { None },
            by_connection_id: Some(connection_id),
        };
        if self.cmd_tx().send(cmd).await.is_err() {
            return Err(anyhow::anyhow!("agent actor not running"));
        }
        if wait {
            rx.await.map_err(|_| anyhow::anyhow!("actor dropped"))
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

    pub async fn usage(&self) -> AnyResult<UsageSnapshot> {
        Ok(self.snapshot().last_usage)
    }

    pub async fn state(&self) -> AnyResult<AgentStateSnapshot> {
        Ok(self.snapshot())
    }

    pub async fn clear(&self, hard: bool) -> AnyResult<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx()
            .send(Command::Clear {
                hard,
                reply: Some(tx),
            })
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))?;
        rx.await.map_err(|_| anyhow::anyhow!("actor dropped"))
    }

    pub async fn compact(&self) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::Compact)
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))
    }

    pub async fn interrupt(&self) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::Interrupt)
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))
    }

    /// §Usage-limits: ask rabbit to drive the `/usage` overlay and
    /// report back the plan-level weekly + 5-hour session limits.
    /// Fire-and-forget — the HTTP handler returns 202 Accepted
    /// immediately; the parsed data arrives on the SSE
    /// `/events/stream` channel a moment later as a fresh
    /// `Usage` envelope carrying the new fields.
    pub async fn usage_check(&self) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::UsageCheck)
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))
    }

    pub async fn restart(&self, fresh: bool) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::Restart { fresh })
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))
    }

    /// Send raw terminal bytes toward rabbit on the given channel
    /// (`TERM_CHAN_CLAUDE` or `TERM_CHAN_SHELL`). Bytes are not gated
    /// on per-tab leadership — every browser and any future bg-task
    /// scheduler pushes into the same FIFO-ordered writer actor, so
    /// concurrent typers interleave at the PTY rather than racing.
    pub async fn send_terminal_bytes(&self, chan: u8, bytes: Bytes) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::SendKeys { chan, data: bytes })
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))?;
        Ok(())
    }

    /// Ask rabbit to force a full TUI redraw by emitting two SIGWINCHs.
    /// Called by the browser WS join path after the bounded replay buffer
    /// has been pushed into a fresh xterm.js pane.
    pub async fn repaint(&self) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::Repaint)
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))?;
        Ok(())
    }

    /// §D Milestone 5 (Phase B): ask rabbit to emit a `ScreenSnapshot`
    /// envelope for the given channel. Called by the browser WS right after
    /// flushing the bounded replay buffer; the resulting snapshot lets the
    /// browser paint an authoritative terminal state, replacing the v1
    /// SIGWINCH-jiggle heuristic.
    pub async fn snapshot_request(&self, chan: u8) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::SnapshotRequest { chan })
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))?;
        Ok(())
    }

    /// Route a programmatic resize through rabbit over the wire
    /// (`EnvelopeBody::Resize` → `PtyCmd::Resize` → `ioctl(TIOCSWINSZ)`
    /// + SIGWINCH). With the per-leader resize model gone, the only
    /// caller is the warren actor's startup path when warren ships
    /// a `TuiConfig` to rabbit.
    pub async fn resize(&self, cols: u16, rows: u16) -> AnyResult<()> {
        self.cmd_tx()
            .send(Command::Resize { cols, rows })
            .await
            .map_err(|_| anyhow::anyhow!("actor not running"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ScreenSnapshotBody, TERM_CHAN_CLAUDE};

    fn h() -> AgentHandle {
        AgentHandle::new(Uuid::nil())
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


    // -----------------------------------------------------------------
    // §A.7 / seq-numbered snapshot protocol — `publish_term` /
    // `subscribe_term` / `replay_term` operate on `TermFrame` (chan,
    // seq, data), not bare bytes. warren must NEVER invent or rewrite a
    // seq on the broadcast path — the same `seq` the rabbit blocking
    // PTY thread assigned has to land on every browser subscriber for
    // the snapshot's `after_seq` watermark to mean anything.
    //
    // The two tests below pin that contract; a future "optimization"
    // that strips the seq (e.g. treating the broadcast payload as
    // pure bytes) would silently break two-step-apply on the browser.
    // -----------------------------------------------------------------

    /// Push a sequence of `TermFrame`s through `publish_term` and
    /// confirm `replay_term()` returns them in order with `chan`/`seq`/
    /// `data` byte-for-byte. The bounded ring (TERM_RING_MAX_CHUNKS =
    /// 128) trims oldest first, so verify the surviving tail still
    /// matches the input exactly.
    #[test]
    fn term_frame_passes_through_publish_and_replay() {
        let h = AgentHandle::new(Uuid::nil());
        // Push 200 frames; only the last 128 must survive the
        // TERM_RING_MAX_CHUNKS bound.
        for i in 1..=200u64 {
            h.publish_term(TermFrame {
                chan: TERM_CHAN_CLAUDE,
                seq: i,
                data: vec![i as u8, (i >> 1) as u8],
            });
        }
        let replayed = h.replay_term();
        assert_eq!(
            replayed.len(),
            128,
            "ring must cap at TERM_RING_MAX_CHUNKS=128 entries"
        );
        // The oldest surviving frame carries seq=200-128+1 = 73.
        let expected_oldest = 200 - 128 + 1;
        assert_eq!(replayed[0].seq, expected_oldest);
        assert_eq!(replayed[0].chan, TERM_CHAN_CLAUDE);
        assert_eq!(
            replayed[0].data,
            vec![expected_oldest as u8, (expected_oldest >> 1) as u8]
        );
        // The newest surviving frame carries seq=200.
        assert_eq!(replayed.last().unwrap().seq, 200);
        // Strict monotonic — every entry exactly +1 from the prior.
        for w in replayed.windows(2) {
            assert_eq!(w[1].seq, w[0].seq + 1);
            assert_eq!(w[1].chan, w[0].chan);
        }
    }

    /// A late-joining subscriber sees the bounded ring in the same
    /// order `publish_term` was called — i.e. the seq ordering is
    /// preserved through the ring so a fresh browser pane that joins
    /// mid-session can replay (or buffer-and-apply on snapshot arrival)
    /// identical data.
    #[tokio::test]
    async fn subscribe_term_receives_term_frame_with_chan_seq_data() {
        let h = AgentHandle::new(Uuid::nil());
        // Subscribe BEFORE publishing so we don't miss any frames.
        let mut rx = h.subscribe_term();
        h.publish_term(TermFrame {
            chan: TERM_CHAN_CLAUDE,
            seq: 7,
            data: b"hello".to_vec(),
        });
        let got = rx.recv().await.expect("subscriber receives TermFrame");
        assert_eq!(got.chan, TERM_CHAN_CLAUDE);
        assert_eq!(got.seq, 7, "seq must ride through untouched");
        assert_eq!(got.data, b"hello");
    }

    /// §A.7 wire-tag lock — the v2 protocol requires the `ScreenSnapshot`
    /// envelope's JSON to carry `"after_seq": <u64>` as a top-level
    /// field so the browser can read the watermark off the envelope and
    /// trim buffered frames whose seq ≤ after_seq before the apply. A
    /// future rename (e.g. `seq_after`, `seq_watermark`) would silently
    /// break the browser-side §4.3 two-step apply with no compile-time
    /// error. Pin the wire shape here so a wire-tag change has to be
    /// intentional.
    #[tokio::test]
    async fn screen_snapshot_envelope_carries_after_seq_to_browser() {
        let h = AgentHandle::new(Uuid::nil());
        let mut rx = h.subscribe_meta();

        // Build a snapshot with a deliberately non-zero after_seq.
        let snap = ScreenSnapshotBody {
            chan: 0x01,
            cols: 80,
            rows: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            text: vec!["".into()],
            after_seq: 42,
        };
        h.publish_meta(EnvelopeBody::ScreenSnapshot(snap.clone()));

        let got = rx.recv().await.expect("subscriber receives ScreenSnapshot");
        // Round-trip through serde to the exact wire shape the
        // browser JS would deserialize.
        let body = match &got {
            EnvelopeBody::ScreenSnapshot(b) => b,
            other => panic!("expected ScreenSnapshot, got {other:?}"),
        };
        assert_eq!(body.after_seq, 42);
        let json = serde_json::to_value(&got).expect("serialize snapshot");
        // Wire tag — must be snake_case "screen_snapshot" so
        // `applyMeta::screen_snapshot` matches.
        assert_eq!(
            json["t"], "screen_snapshot",
            "wire tag must match `t: \"screen_snapshot\"`"
        );
        // The new field must be present and have the value we set.
        assert_eq!(
            json["after_seq"], 42,
            "wire JSON must carry after_seq as a top-level number"
        );
    }
}
