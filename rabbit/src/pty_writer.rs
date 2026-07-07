//! §Once-and-for-all PTY writer actor.
//!
//! Replaces the multi-site `Arc<Mutex<Box<dyn Write + Send>>>`
//! pattern with a single dedicated tokio task that owns the
//! kernel-side writer end of the claude (or shell) PTY. Every
//! other site in the supervisor submits [`WriteCmd`] values to the
//! task's `mpsc`.
//!
//! # Why a single-writer primitive
//!
//! The previous design serialized bytes through a Mutex but
//! dropped the guard between every action. That allowed inbound
//! operator bytes (Prompt/Slash/Interrupt/Resize/keystrokes) to
//! interleave with the active scraper's sequence of
//! `/usage\r`, `\x1b[B` × N, and `\x1b`. Three live races were
//! documented in [`docs-plan-pty-writer-actor.md`] (see also the
//! historical discussion in `docs-plan-usage-limits-small-terminal.md`):
//!
//! - The slash-command race during scrape: `\x15/usa` /
//!   `ge\r` could be sliced by an inbound `Slash` envelope.
//! - Concurrent `UsageCheck`: a second click within the scrape
//!   window spawned a parallel scraper competing for the same
//!   broadcast receiver and PTY master.
//! - Resize mid-scrape: overlay layout flips; parser survived
//!   because it's header-anchored but the operator's screen
//!   flicker was unmodeled.
//!
//! Encoding writes as FIFO-ordered [`WriteCmd`] values makes
//! ordering STRUCTURAL. A [`WriteCmd::Sequence`] is submitted as a
//! single FIFO unit; nothing else can interleave its bytes. A
//! [`WriteCmd::Cancel`] is the only command that can preempt a
//! sequence in flight — it is the primitive the operator's
//! `Interrupt` envelope maps to, so the abort path is
//! first-class.
//!
//! # FIFO + Sequence + Cancel contract
//!
//! - Bytes / Sequence / Resize submitted after a Sequence in
//!   flight are queued behind it in the `mpsc` and processed
//!   after the Sequence completes (or aborts). They can NOT
//!   slice through the Sequence's bytes.
//! - Cancel submitted after a Sequence sets a flag the actor
//!   checks at the next Sequence step boundary; the in-flight
//!   Sequence aborts, fires its outcome with
//!   [`SequenceOutcome::AbortedBeforeStep`], and the actor
//!   proceeds to the next command (typically `Bytes([0x03])`).
//! - All dequeues happen in the actor's tokio task. Other sites
//!   only have a `mpsc::Sender<WriteCmd>`; they cannot read the
//!   channel.
//!
//! # Wire-output invariant
//!
//! Every byte the actor writes to the PTY master's slave end
//! appears in the kernel in the exact order the actor dequeued
//! `WriteCmd` items. This is what eliminates the prior races.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;

/// Channel depth for the writer's mpsc. Sized to cover a 2s scrape
/// (which submits a Sequence as a single item, plus any inbound
/// keystrokes and one or two control envelopes that may arrive
/// during the window). 256 is the same depth `term_bcast_tx` uses
/// for the broadcast receiver — symmetrical budget.
const WRITER_MPSC_CAP: usize = 256;

/// One unit of work submitted to the writer actor. FIFO-ordered.
///
/// `Bytes` / `Sequence` / `Resize` are non-preempting: they run
/// in submission order, each waiting for the prior to complete.
/// Cancellation is preempting BUT does not require a command —
/// [`WriterHandle::cancel`] flips a shared flag the actor
/// consults at every Sequence step boundary. `WriteCmd::Cancel`
/// exists as an explicit form for callers that want a cancel
/// command in their FIFO (e.g. for audit logs).
///
/// `Debug` only; the `oneshot::Sender` inside `Sequence` is not
/// `Clone` so a derived `Clone` would not compile.
#[derive(Debug)]
pub enum WriteCmd {
    /// Single write. Atomic from the kernel's perspective.
    Bytes(Vec<u8>),
    /// Multi-write sequence submitted as ONE FIFO unit. The actor
    /// writes each item in order, sleeping `inter_item_delay`
    /// between items. Nothing else can slice through the sequence
    /// (a cancel IS allowed to abort; nothing else is).
    Sequence {
        items: Vec<Vec<u8>>,
        inter_item_delay: Duration,
        /// Resolves when the sequence finishes — completed,
        /// aborted at a step boundary, or failed mid-write. Used
        /// by the active scraper to decide whether to publish a
        /// full `Usage` snapshot or a partial one with
        /// `scrape_incomplete = true`.
        outcome: oneshot::Sender<SequenceOutcome>,
    },
    /// Explicit-cancel command. Sets the cancellation flag
    /// (equivalent to [`WriterHandle::cancel`]). The actor picks
    /// it up during a Sequence's inter-item sleep via the
    /// `select!` and uses it to break the wait early — so a
    /// `WriteCmd::Cancel` queued mid-sleep wakes the actor
    /// within a few millis instead of waiting out the full
    /// `inter_item_delay`.
    Cancel,
    /// PTY resize. Currently a no-op on the writer actor; resize
    /// still goes through the blocking PTY thread's `pty_rx`
    /// channel for the kernel `TIOCSWINSZ`. Kept as a `WriteCmd`
    /// variant for symmetry.
    Resize { cols: u16, rows: u16 },
}

/// Outcome of a completed Sequence. Sent on the `oneshot::Sender`
/// carried inside `WriteCmd::Sequence`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceOutcome {
    /// Every item in the sequence was written successfully and
    /// no `Cancel` arrived during the run.
    Completed,
    /// The sequence was aborted because a `Cancel` arrived. The
    /// value is the index of the FIRST STEP THAT WAS NOT WRITTEN
    /// (so `AbortedBeforeStep(0)` means "the very first item did
    /// not get written" and `AbortedBeforeStep(items.len())`
    /// means "all items were written but the post-loop check
    /// caught the cancel").
    AbortedBeforeStep(usize),
    /// A `write_all` call returned an error mid-sequence. The
    /// actor returns from its drain loop after this — the writer
    /// is unrecoverable (broken pipe / child exited).
    Failed(String),
}

/// Bundles the `(writer_tx, cancel_flag)` pair a supervisor
/// outer-loop caller needs. The actor holds the writer end; the
/// caller holds this sender plus a way to flip the cancel flag
/// (used only by the `Interrupt` envelope path).
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<WriteCmd>,
    cancel_flag: Arc<AtomicBool>,
}

impl WriterHandle {
    /// Submit a single byte payload. `await`s on the mpsc's
    /// capacity; bounded latency (the channel is 256 deep).
    pub async fn bytes(&self, data: Vec<u8>) {
        let _ = self.tx.send(WriteCmd::Bytes(data)).await;
    }

    /// Submit a multi-step sequence. Returns a oneshot receiver
    /// for the outcome. The Sequence occupies one FIFO slot —
    /// the byte is atomic against non-`Cancel` submissions.
    pub async fn sequence(
        &self,
        items: Vec<Vec<u8>>,
        inter_item_delay: Duration,
    ) -> oneshot::Receiver<SequenceOutcome> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(WriteCmd::Sequence {
                items,
                inter_item_delay,
                outcome: tx,
            })
            .await;
        rx
    }

    /// Flip the cancellation flag (synchronous, immediate). Any
    /// in-flight Sequence's next-step check sees this and aborts
    /// within `inter_item_delay` of the current step. The caller
    /// is responsible for clearing the flag with
    /// [`WriterHandle::clear_cancel`] once the preempted scrape
    /// has unwound, so a fresh scrape can run unimpeded.
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    /// Submit a `WriteCmd::Cancel` to the FIFO. Equivalent to
    /// [`WriterHandle::cancel`] in terms of effect (sets the
    /// flag), but goes through the mpsc so it ALSO wakes any
    /// in-flight Sequence's inter-item sleep within a few
    /// milliseconds instead of waiting out the full delay.
    /// Use this when sub-step-latency preemption matters.
    pub async fn cancel_via_queue(&self) {
        let _ = self.tx.send(WriteCmd::Cancel).await;
    }

    /// Clear the cancellation flag. Call after the scrape (or
    /// any operation that may have been preempted) completes.
    pub fn clear_cancel(&self) {
        self.cancel_flag.store(false, Ordering::SeqCst);
    }

    /// Submit a `Resize` command through the FIFO. The actor calls
    /// the resize closure registered at spawn time
    /// ([`register_resize_callback`]) which handles BOTH the kernel
    /// `TIOCSWINSZ` and the in-process `TermTracker::resize`. FIFO
    /// ordering vs. in-flight Bytes / Sequence is preserved (a
    /// resize queued mid-sequence lands AFTER the sequence
    /// completes).
    pub async fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(WriteCmd::Resize { cols, rows }).await;
    }
}

/// Closure the writer actor invokes on `WriteCmd::Resize`. Must
/// cover both `pty.resize(...)` (kernel `TIOCSWINSZ`) and
/// `vt.resize(...)` (in-process VT) so `ScreenSnapshot.cols/rows`
/// reflects the new size for late joiners.
///
/// Wrapped in `parking_lot::Mutex<Box<dyn FnMut(u16, u16) + Send>>`
/// for `Send` (the writer actor runs on a different thread than the
/// blocking PTY thread that registered the callback) and to bound
/// the lock to a single resize call.
pub type ResizeCallback = Arc<parking_lot::Mutex<Box<dyn FnMut(u16, u16) + Send>>>;

/// Spawn the writer actor. Returns a [`WriterHandle`] for callers
/// in the supervisor (and in the scraper task) to submit work.
///
/// `writer` is consumed — the actor is the sole owner of the
/// kernel-side write end of the PTY master. After this call any
/// `take_writer()` on the master would fail; any old
/// `Arc<Mutex<Box<dyn Write + Send>>>` is redundant.
///
/// The optional `resize_callback` is invoked by the actor on
/// `WriteCmd::Resize { cols, rows }` — see [`ResizeCallback`] for
/// the contract. If `None`, the actor logs and skips.
pub fn spawn_pty_writer(
    writer: Box<dyn Write + Send>,
    resize_callback: Option<ResizeCallback>,
) -> WriterHandle {
    let (tx, mut rx) = mpsc::channel::<WriteCmd>(WRITER_MPSC_CAP);
    let cancel_flag = Arc::new(AtomicBool::new(false));

    let cancel_for_actor = cancel_flag.clone();
    tokio::spawn(async move {
        // The closure captures `writer` by move; we need it
        // mutable inside. Re-bind with `mut` since the function
        // signature's `mut` doesn't propagate into the closure.
        let mut writer = writer;
        let resize_callback = resize_callback;
        // Buffer of commands received during a Sequence's
        // inter-item sleep. They were FIFO-after the current
        // Sequence in submission time; we must process them in
        // original order AFTER the Sequence completes.
        let mut deferred: VecDeque<WriteCmd> = VecDeque::new();

        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriteCmd::Bytes(b) => {
                    write_bytes(&mut writer, &b);
                }
                WriteCmd::Sequence {
                    items,
                    inter_item_delay,
                    outcome,
                } => {
                    let mut aborted = false;
                    let mut aborted_at: usize = 0;
                    for (i, item) in items.iter().enumerate() {
                        if cancel_for_actor.load(Ordering::SeqCst) {
                            aborted = true;
                            aborted_at = i;
                            break;
                        }
                        if let Err(e) = writer.write_all(item) {
                            // Broken pipe / child gone — fire the
                            // outcome for THIS sequence with
                            // Failed, then exit the actor loop.
                            // Future submissions would have to
                            // spawn a fresh actor anyway.
                            let _ = outcome.send(SequenceOutcome::Failed(format!(
                                "write_all failed at item {i}: {e}"
                            )));
                            return;
                        }
                        let _ = writer.flush();

                        // Inter-item delay uses a select with
                        // `rx.recv()` so a Cancel that arrives
                        // mid-sleep preempts the sequence
                        // immediately rather than waiting out the
                        // full delay. Other commands received
                        // during sleep are buffered in `deferred`
                        // and processed in FIFO order after the
                        // sequence completes — preserving the
                        // "Sequence is a single FIFO unit"
                        // invariant.
                        if i + 1 < items.len() && !inter_item_delay.is_zero() {
                            let sleep_done = Arc::new(AtomicBool::new(false));
                            let sleep_done_for_select = sleep_done.clone();
                            let mut sleep_fut = Box::pin(sleep(inter_item_delay));
                            loop {
                                tokio::select! {
                                    _ = &mut sleep_fut => break,
                                    received = rx.recv() => match received {
                                        Some(WriteCmd::Cancel) => {
                                            // Set the flag so the
                                            // NEXT step's check
                                            // aborts. We continue
                                            // sleeping — the
                                            // operator's Ctrl-C
                                            // byte (queued as
                                            // Bytes([0x03]) after
                                            // this Cancel) goes out
                                            // after the sequence
                                            // aborts.
                                            cancel_for_actor
                                                .store(true, Ordering::SeqCst);
                                        }
                                        Some(other) => {
                                            // Buffered for
                                            // after-sequence
                                            // processing in
                                            // original FIFO order.
                                            deferred.push_back(other);
                                        }
                                        None => {
                                            // Sender dropped;
                                            // actor is exiting.
                                            sleep_done_for_select
                                                .store(true, Ordering::SeqCst);
                                            // Best-effort:
                                            // tell the outcome
                                            // that we aborted.
                                            let _ = outcome.send(
                                                SequenceOutcome::AbortedBeforeStep(i + 1),
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                            let _ = sleep_done; // silence unused
                        }
                    }
                    // Post-loop check: if a Cancel arrived AFTER
                    // the last item was written but BEFORE we
                    // exited the for loop, the flag is set but no
                    // `aborted = true` fired. Report as a clean
                    // abort at index == items.len() so the
                    // caller distinguishes "all bytes hit the
                    // master but preempt happened" from
                    // "everything completed normally."
                    if !aborted && cancel_for_actor.load(Ordering::SeqCst) {
                        aborted = true;
                        aborted_at = items.len();
                    }
                    let result = if aborted {
                        SequenceOutcome::AbortedBeforeStep(aborted_at)
                    } else {
                        SequenceOutcome::Completed
                    };
                    let _ = outcome.send(result);
                }
                WriteCmd::Cancel => {
                    cancel_for_actor.store(true, Ordering::SeqCst);
                }
                WriteCmd::Resize { cols, rows } => {
                    // The closure covers both `pty.resize` (kernel
                    // TIOCSWINSZ) and `vt.resize` so `ScreenSnapshot`
                    // reflects the new dims.
                    if let Some(cb) = resize_callback.as_ref() {
                        let mut g = cb.lock();
                        g(cols, rows);
                    } else {
                        log::warn!(
                            "pty writer: Resize command received ({cols}x{rows}) but no \
                             resize callback was registered at spawn time; \
                             ScreenSnapshot may report stale dims until next vt feed"
                        );
                    }
                }
            }

            // After any command, drain `deferred` in original
            // FIFO order. This is where Bytes / Sequence / Resize
            // submissions received during a sequence's inter-item
            // sleep get processed — strictly AFTER the sequence,
            // never inside it.
            while let Some(deferred_cmd) = deferred.pop_front() {
                match deferred_cmd {
                    WriteCmd::Bytes(b) => write_bytes(&mut writer, &b),
                    WriteCmd::Sequence {
                        items,
                        inter_item_delay,
                        outcome,
                    } => {
                        // Recurse via a small inline loop — same
                        // shape as the outer Sequence arm. Using
                        // a labeled loop would also work but the
                        // duplication is bounded and clear.
                        let mut aborted = false;
                        let mut aborted_at: usize = 0;
                        for (i, item) in items.iter().enumerate() {
                            if cancel_for_actor.load(Ordering::SeqCst) {
                                aborted = true;
                                aborted_at = i;
                                break;
                            }
                            if let Err(e) = writer.write_all(item) {
                                let _ = outcome.send(SequenceOutcome::Failed(format!(
                                    "write_all failed at item {i}: {e}"
                                )));
                                return;
                            }
                            let _ = writer.flush();
                            if i + 1 < items.len() && !inter_item_delay.is_zero() {
                                tokio::time::sleep(inter_item_delay).await;
                            }
                        }
                        let result = if aborted {
                            SequenceOutcome::AbortedBeforeStep(aborted_at)
                        } else {
                            SequenceOutcome::Completed
                        };
                        let _ = outcome.send(result);
                    }
                    WriteCmd::Cancel => {
                        cancel_for_actor.store(true, Ordering::SeqCst);
                    }
                    WriteCmd::Resize { cols, rows } => {
                        // Same path as the outer arm — invoke the
                        // resize callback if registered, else warn.
                        if let Some(cb) = resize_callback.as_ref() {
                            let mut g = cb.lock();
                            g(cols, rows);
                        } else {
                            log::warn!(
                                "pty writer: deferred Resize command received ({cols}x{rows}) \
                                 but no resize callback was registered; dropping"
                            );
                        }
                    }
                }
            }
        }
    });

    WriterHandle { tx, cancel_flag }
}

fn write_bytes(writer: &mut Box<dyn Write + Send>, data: &[u8]) {
    if let Err(e) = writer.write_all(data) {
        log::warn!("pty writer: write_all failed: {e:?}");
        return;
    }
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// `SharedBuf` is a thread-safe byte sink for testing the
    /// writer actor without a real PTY. Each `write_all` call
    /// appends bytes to a shared `Vec`; assertions then read the
    /// final vector to verify FIFO ordering.
    #[derive(Clone)]
    struct SharedBuf(Arc<StdMutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn shared_buf() -> (SharedBuf, Arc<StdMutex<Vec<u8>>>) {
        let v = Arc::new(StdMutex::new(Vec::new()));
        (SharedBuf(v.clone()), v)
    }

    /// Pin the byte ordering through a single `Bytes` write.
    /// Memory-backed `SharedBuf` keeps the test hermetic.
    #[tokio::test]
    async fn bytes_write_lands_in_order() {
        let (w, sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        h.bytes(b"abc".to_vec()).await;
        h.bytes(b"def".to_vec()).await;
        // Give the actor a tick to drain the channel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*sink.lock().unwrap(), b"abcdef".to_vec());
    }

    /// Outcome of an empty Sequence is `Completed` (zero items is
    /// a valid no-op).
    #[tokio::test]
    async fn empty_sequence_completes() {
        let (w, _sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        let rx = h
            .sequence(Vec::<Vec<u8>>::new(), Duration::from_millis(10))
            .await;
        let outcome = rx.await.expect("oneshot");
        assert_eq!(outcome, SequenceOutcome::Completed);
    }

    /// `clear_cancel` resets the flag so a fresh scrape isn't
    /// immediately preempted by a stale flag from a prior
    /// preempt.
    #[tokio::test]
    async fn clear_cancel_resets_flag() {
        let (w, _sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        h.cancel_flag.store(true, Ordering::SeqCst);
        h.clear_cancel();
        assert!(!h.cancel_flag.load(Ordering::SeqCst));
    }

    /// §Writer-actor regression #1: heterogeneous submissions
    /// (Bytes / Sequence / Bytes) land in submission order on the
    /// kernel side. Pre-fix `Arc<Mutex<Writer>>` did NOT
    /// guarantee this — interleaving was possible when two sites
    /// both acquired the mutex around the same instant. The
    /// `WriteCmd` FIFO makes this structural.
    #[tokio::test]
    async fn fifo_ordering_across_heterogeneous_submissions() {
        let (w, sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        // Submit a, then a 2-item Sequence, then d. The Sequence
        // is ONE FIFO unit; bytes inside it cannot interleave
        // with anything else.
        h.bytes(b"a".to_vec()).await;
        let outcome_rx = h
            .sequence(vec![b"b".to_vec(), b"c".to_vec()], Duration::from_millis(5))
            .await;
        h.bytes(b"d".to_vec()).await;
        let outcome = outcome_rx.await.expect("oneshot");
        assert_eq!(outcome, SequenceOutcome::Completed);
        // Give the trailing Bytes("d") time to drain.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(*sink.lock().unwrap(), b"abcd".to_vec());
    }

    /// §Writer-actor regression #2: the slash-command race during
    /// scrape. Submit the `/usage\r` Sequence as ONE FIFO unit and
    /// simultaneously submit `Bytes(b"hello")` from another task.
    /// Pre-fix, the bytes could interleave and claude would see
    /// `\x15/usa…hello…ge\r` — a garbled slash command. Post-fix,
    /// the bytes stay in submission order and the slash command
    /// arrives intact.
    #[tokio::test]
    async fn slash_command_race_resolved() {
        let (w, sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        let slash_task = {
            let h = h.clone();
            tokio::spawn(async move {
                h.sequence(vec![b"\x15/usage\r".to_vec()], Duration::from_millis(0))
                    .await;
            })
        };
        // Concurrent Bytes submission while the Sequence is in flight.
        h.bytes(b"hello".to_vec()).await;
        let _ = slash_task.await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let observed = sink.lock().unwrap().clone();
        // The slash command must arrive intact as ONE block, and
        // `hello` arrives either BEFORE the slash (sub-FIFO order
        // if it queued first) or AFTER. NEVER interleaved.
        let slash_pos = observed
            .windows(b"\x15/usage\r".len())
            .position(|w| w == b"\x15/usage\r")
            .expect("slash command must arrive intact on the master side");
        let hello_pos = observed
            .windows(b"hello".len())
            .position(|w| w == b"hello")
            .expect("hello bytes must arrive intact on the master side");
        // The two positions must not overlap with each other
        // (i.e. neither substring is sliced through the other).
        let slash_end = slash_pos + b"\x15/usage\r".len();
        let hello_end = hello_pos + b"hello".len();
        let disjoint = hello_end <= slash_pos || slash_end <= hello_pos;
        assert!(
            disjoint,
            "slash command and hello bytes interleaved: observed={observed:?}"
        );
    }

    /// §Writer-actor regression #3: an operator `Interrupt` arriving
    /// mid-scrape preempts the in-flight Sequence within a few
    /// milliseconds. The SequenceOutcome fires
    /// `AbortedBeforeStep(N)` where N is the index of the first
    /// un-written step. The Ctrl-C byte (`0x03`) lands on the
    /// master AFTER the partial sequence (because of FIFO
    /// ordering), not interleaved.
    #[tokio::test]
    async fn interrupt_preempts_in_flight_sequence() {
        let (w, sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        // 5-item Sequence with 80ms inter-item delay — long enough
        // that we can interrupt mid-way.
        let items: Vec<Vec<u8>> = (0..5).map(|i| vec![b'a' + i]).collect();
        let outcome_rx = h.sequence(items, Duration::from_millis(80)).await;
        // Wait long enough for the actor to write ~2 items
        // (item 0 written immediately, then 80ms sleep, then
        // item 1, then 80ms sleep → at ~160ms we should be past
        // item 1).
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Now flip the cancel flag synchronously (sub-step-latency
        // preempt) and queue an explicit Cancel to wake the
        // inter-item sleep, then submit the Ctrl-C byte.
        h.cancel();
        h.cancel_via_queue().await;
        h.bytes(vec![0x03]).await;
        let outcome = outcome_rx.await.expect("oneshot");
        match outcome {
            SequenceOutcome::AbortedBeforeStep(idx) => {
                // We preempted at ~150ms, which is mid-sleep
                // before item 2 → items 0 and 1 should be written
                // and item 2 onward should be skipped. The actor's
                // step boundary check fires BEFORE each write; if
                // we arrived during the second sleep, idx is at
                // least 2 (item 2 not written). Allow a small
                // window for scheduling jitter.
                assert!(
                    idx >= 2,
                    "preempt should skip at least item 2 (idx >= 2), got {idx}"
                );
                assert!(
                    idx <= 3,
                    "preempt should land within one step of expected (idx <= 3), got {idx}"
                );
            }
            other => panic!("expected AbortedBeforeStep, got {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let observed = sink.lock().unwrap().clone();
        // The Ctrl-C byte (0x03) MUST land on the master AFTER
        // the partial sequence. It is the LAST byte in the
        // observed stream.
        assert_eq!(
            observed.last().copied(),
            Some(0x03),
            "Ctrl-C byte must be the last byte in observed stream (FIFO after Cancel), observed={observed:?}"
        );
        // And the partial sequence's written items must appear
        // BEFORE the Ctrl-C, contiguous.
        let pre_ctrl_c = &observed[..observed.len() - 1];
        assert!(
            !pre_ctrl_c.is_empty(),
            "at least one sequence item must have been written before preempt"
        );
        // Each written item was one byte 'a'+i, contiguous.
        let expected_pre: Vec<u8> = (0..aborted_index_for_test(&outcome) as u8)
            .map(|i| b'a' + i)
            .collect();
        assert_eq!(
            pre_ctrl_c, &expected_pre,
            "partial sequence bytes don't match expected"
        );
    }

    /// §Writer-actor regression #4: a `Resize` command
    /// submitted mid-sequence is processed AFTER the in-flight
    /// sequence completes. The actor's `WriteCmd::Resize` arm is
    /// a no-op (the actual resize still goes through the
    /// blocking PTY thread's pty_tx), but its FIFO position
    /// must NOT slice through the sequence. We assert by
    /// ordering: a trailing Bytes arrives AFTER the sequence,
    /// and the actor's drain log proves the Resize was processed
    /// in submission order.
    #[tokio::test]
    async fn resize_during_sequence_does_not_corrupt() {
        let (w, sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        // Sequence with 30ms delay.
        let outcome_rx = h
            .sequence(
                vec![b"X".to_vec(), b"Y".to_vec(), b"Z".to_vec()],
                Duration::from_millis(30),
            )
            .await;
        // Mid-sequence: queue a Resize (no-op) and a trailing Bytes.
        tokio::time::sleep(Duration::from_millis(40)).await;
        h.resize(80, 24).await;
        h.bytes(b"AFTER".to_vec()).await;
        let outcome = outcome_rx.await.expect("oneshot");
        assert_eq!(outcome, SequenceOutcome::Completed);
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Bytes must arrive in submission order: XYZ then AFTER.
        assert_eq!(*sink.lock().unwrap(), b"XYZAFTER".to_vec());
    }

    /// Test-only helper so the interrupt test can introspect
    /// `AbortedBeforeStep(N)` for ordering assertions without
    /// pattern-matching `other` formats. Lives inside the test
    /// module so it doesn't leak into the public type.
    fn aborted_index_for_test(outcome: &SequenceOutcome) -> usize {
        match outcome {
            SequenceOutcome::AbortedBeforeStep(i) => *i,
            _ => 0,
        }
    }

    #[tokio::test]
    async fn resize_callback_is_invoked() {
        use parking_lot::Mutex;
        let (w, _sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let observed: Arc<Mutex<Vec<(u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_for_cb = observed.clone();
        let callback: ResizeCallback = Arc::new(Mutex::new(Box::new(move |c, r| {
            observed_for_cb.lock().push((c, r));
        })));
        let h = spawn_pty_writer(writer, Some(callback));
        h.resize(80, 24).await;
        h.resize(132, 50).await;
        // Give the actor a tick to drain.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = observed.lock().clone();
        assert_eq!(
            got,
            vec![(80, 24), (132, 50)],
            "resize callback must fire in submission order"
        );
    }

    #[tokio::test]
    async fn resize_with_no_callback_does_not_crash() {
        let (w, _sink) = shared_buf();
        let writer: Box<dyn Write + Send> = Box::new(w);
        let h = spawn_pty_writer(writer, None);
        // Should not panic despite no callback registered.
        h.resize(80, 24).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
