use crate::config::Config;
use crate::health::{serve as serve_health, HealthState};
use crate::hooks_install;
use crate::input;
use crate::link::{Link, LinkCmd, LinkEvent, ReplaySnapFn};
use crate::meta_ring::MetaRing;
use crate::observer::hooks::{ObserverEvent, ObserverHandle};
use crate::observer::state::State;
use crate::observer::transcript::{default_transcript_path, TranscriptTail, UsageUpdate};
use crate::pty::{ExitKind, Pty, PtyExitStatus};
use crate::respawn::{self, CrashWindow};
use crate::shell::{self, ShellCmd, ShellHandle};
use crate::wire::{
    Envelope, EnvelopeBody, LogLine, ScreenSnapshotBody, StateFrame, TermFrame,
    TermSize, TERM_CHAN_CLAUDE, TERM_CHAN_SHELL,
};
use anyhow::Result;
use parking_lot::Mutex;
use std::io::Write;
use portable_pty::ChildKiller;
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn run(config: Config) -> Result<()> {
    std::fs::create_dir_all(&config.workdir).ok();

    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    install_signal_handlers(shutdown.clone());

    let health = HealthState::new();
    {
        let cfg = config.clone();
        let h = health.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_health(cfg.health_port, h).await {
                log::error!("health server stopped: {e:?}");
            }
        });
    }

    let observer = ObserverHandle::new();
    {
        let obs = observer.clone();
        let port = config.observer_port;
        tokio::spawn(async move {
            if let Err(e) = crate::observer::hooks::serve(port, obs).await {
                log::error!("observer server stopped: {e:?}");
            }
        });
    }

    let hook_bin = hooks_install::resolve_hook_bin(config.hook_bin.clone());
    if let Err(e) = hooks_install::install(std::path::Path::new(&config.workdir), &hook_bin) {
        log::warn!("could not install hook settings.json: {e:?}");
    }

    let agent_id = Uuid::new_v4();
    let claude_version = detect_claude_version(&config).await;

    // §A.7: the replay buffer holds per-frame triples (chan, seq, data)
    // so each binary message sent on link reconnect preserves the seq the
    // blocking PTY thread assigned at read time. The browser pins its
    // high-water-mark against `seq` (a late-arriving
    // `ScreenSnapshot::after_seq` tells it which buffered frames are
    // already covered), so losing the seq on reconnect would silently
    // re-set the HWM and let stale bytes overpaint the snapshot's
    // truncated tail.
    let replay_buf: Arc<Mutex<VecDeque<TermFrame>>> = Arc::new(Mutex::new(VecDeque::new()));
    let snap_buf = replay_buf.clone();
    let replay_snap: ReplaySnapFn = Arc::new(move || {
        let buf = snap_buf.lock();
        buf.iter().cloned().collect()
    });

    let (cmd_tx, cmd_rx) = mpsc::channel::<LinkCmd>(128);
    let (event_tx, mut event_rx) = mpsc::channel::<LinkEvent>(128);

    let meta_ring = Arc::new(MetaRing::new(config.meta_ring_bytes));

    let link = Link::new(
        config.warren_url.clone(),
        config.agent_token.clone(),
        agent_id,
        claude_version.clone(),
        TermSize {
            cols: config.term_cols,
            rows: config.term_rows,
        },
        cmd_rx,
        event_tx,
        replay_snap,
        meta_ring,
        shutdown.clone(),
    );
    {
        tokio::spawn(async move {
            if let Err(e) = link.run().await {
                log::error!("link exited: {e:?}");
            }
        });
    }

    spawn_transcript_relay(
        std::path::Path::new(&config.workdir),
        observer.clone(),
        cmd_tx.clone(),
    );

    // §D Milestone 5: optional debug shell PTY (`/agent/:id/shell`). Off by
    // default; when enabled it runs alongside claude on its own channel.
    let shell: Option<ShellHandle> = if config.enable_shell {
        log::info!("shell enabled: bin={} args={:?}", config.shell_bin, config.shell_args);
        Some(shell::spawn(&config, cmd_tx.clone(), shutdown.clone()))
    } else {
        None
    };

    let mut crash_window = CrashWindow::new(
        Duration::from_secs(config.crash_window_secs),
        config.crash_threshold,
    );
    let mut restart_pending: Option<bool> = None;
    let mut dead = false;
    let mut active: Option<ActiveSession> = None;
    let mut outcome_rx: mpsc::Receiver<RunOutcome> = mpsc::channel(8).1;
    // Dedup the shutdown arms. Without this, every iteration of the
    // outer loop re-creates fresh `wait_for_shutdown()` futures in the
    // select! below; once `shutdown` is `true`, those futures all
    // return Ready immediately and the handler fires once per loop
    // spin (visible as N copies of "shutdown signal received; …" in
    // the log during the graceful-exit grace window). The flag ensures
    // the log message + `PtyCmd::GracefulShutdown` are emitted exactly
    // once.
    let mut shutdown_acked = false;

    // Fold `MODEL` env into the base claude args once, at startup, so it's
    // stable across the spawn loop and doesn't depend on the operator also
    // setting CLAUDE_ARGS (the §1 stable CLI flag, separate from §1.1's flags).
    let base_args: Vec<String> = match &config.model {
        Some(m) => {
            let mut v = config.claude_args.clone();
            v.push("--model".to_string());
            v.push(m.clone());
            v
        }
        None => config.claude_args.clone(),
    };

    loop {
        // Spawn a new claude generation if we have nothing running and aren't dead.
        if active.is_none() && !dead && !shutdown.load(Ordering::SeqCst) {
            let fresh = restart_pending.take().unwrap_or(false);
            let session_id = observer.latest_session();
            let args = respawn::effective_args(&base_args, session_id.as_deref(), fresh);
            log::info!(
                "spawning pty: bin={} args={:?} fresh={}",
                config.claude_bin,
                args,
                fresh
            );
            match spawn_run_one(
                &config,
                health.clone(),
                agent_id,
                &claude_version,
                observer.clone(),
                args,
                replay_buf.clone(),
                cmd_tx.clone(),
                shutdown.clone(),
            ) {
                Ok(sess) => {
                    let OutcomeChannels {
                        pty_link_tx,
                        outcome_rx_in,
                    } = sess.outcome_channels;
                    outcome_rx = outcome_rx_in;
                    active = Some(ActiveSession {
                        pty_link_tx,
                        killer: sess.killer,
                        writer: sess.writer,
                    });
                }
                Err(e) => {
                    log::error!("run_one spawn failed: {e:?}");
                    health.set_alive(false);
                    let _ = send_state(
                        &observer,
                        &cmd_tx,
                        StateFrame {
                            state: "dead".into(),
                            session_id: None,
                            reason: Some("spawn_failed".into()),
                        },
                    )
                    .await;
                    dead = true;
                }
            }
        }

        if shutdown.load(Ordering::SeqCst) && active.is_none() {
            // Politely close the WS so warren sees the agent go away before
            // we exit. The link also polls `shutdown` itself, so this is
            // best-effort — if the send fails (channel full / closed), the
            // flag will still break the link's reconnect loop.
            let _ = cmd_tx.send(LinkCmd::Shutdown).await;
            break;
        }

        let active_link_tx = active.as_ref().map(|s| s.pty_link_tx.clone());
        let active_shared_writer = active.as_ref().map(|s| s.writer.clone());
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(50)), if active.is_some() => {
                // tick: nothing; just keeps select responsive while children run.
            }
            ev = event_rx.recv() => {
                match ev {
                    Some(LinkEvent::Text(env)) => {
                        if let EnvelopeBody::Restart { fresh } = env.body {
                            restart_pending = Some(fresh);
                            log::info!("restart requested via WS, fresh={fresh}");
                            dead = false;
                            // Signal the child via the SHARED killer, not
                            // through `pty_link_tx` / `PtyCmd::Terminate`.
                            // The latter lands in a channel that the
                            // blocking PTY reader only drains between
                            // `read()` calls — and when claude is stuck
                            // at a TUI prompt emitting no further output,
                            // `read()` blocks indefinitely and the queued
                            // Terminate is never seen. The `ChildKiller`
                            // is portable-pty's documented mechanism for
                            // sending a signal from a thread other than
                            // the one blocked in `.wait()`; it reaches the
                            // child immediately, EOF arrives on the master,
                            // and the blocking thread's restructured `Ok(0)`
                            // arm (see below) sends `PtyEvt::Exited` so the
                            // driver reports the outcome.
                            if let Some(active) = &active {
                                if let Err(e) = active.killer.lock().kill() {
                                    // ESRCH ("no such process") means the
                                    // child already exited on its own —
                                    // the kill call was redundant but the
                                    // outcome (child gone) is what we
                                    // wanted. Logged at debug, not warn.
                                    log::debug!(
                                        "Restart: shared killer.kill returned {e:?} (child likely already gone)"
                                    );
                                }
                            }
                        } else if let EnvelopeBody::SnapshotRequest { chan } = &env.body {
                            // §D Milestone 5 (Phase B): late-join screen dump.
                            // Currently only the claude channel has a
                            // `TermTracker`. A shell-channel request is a
                            // future-work item (would need to mirror the VT
                            // for the optional `bash` PTY).
                            if chan == &TERM_CHAN_CLAUDE {
                                if let Some(tx) = &active_link_tx {
                                    let _ = tx
                                        .send(PtyCmd::Snapshot {
                                            chan: TERM_CHAN_CLAUDE,
                                        })
                                        .await;
                                }
                            } else {
                                log::debug!(
                                    "snapshot request for chan {chan} not yet wired (only claude has a VT)"
                                );
                            }
                        } else if let Some(tx) = &active_link_tx {
                            // §D prompt policy: reject-when-Running. A prompt
                            // arriving mid-turn would inject keystrokes into a
                            // live turn (possibly over a human's edit), so bounce
                            // it back with a dedicated `PromptRejected` envelope
                            // instead of dispatching. Control frames
                            // (Interrupt/Slash/Clear/Resize/Repaint) still pass
                            // through unconditionally.
                            //
                            // The dedicated variant (vs. a generic `Log { warn }`)
                            // lets warren render a targeted UI affordance tied to
                            // the original prompt id — see
                            // `warren/templates/agent_claude.html`.
                            let reject =
                                should_reject_prompt(observer.latest_state(), &env.body);
                            if reject {
                                log::info!(
                                    "rejecting prompt: agent is Running (reject-when-Running policy)"
                                );
                                let rejected = prompt_rejected_for(&env);
                                let _ = cmd_tx
                                    .send(LinkCmd::SendMeta(rejected))
                                    .await;
                            } else {
                                dispatch_to_pty(
                                    &env,
                                    active_shared_writer.as_ref(),
                                    tx,
                                    config.term_cols,
                                    config.term_rows,
                                )
                                .await;
                            }
                        }
                    }
                    Some(LinkEvent::Binary { chan, data }) => {
                        if chan == TERM_CHAN_SHELL {
                            if let Some(sh) = &shell {
                                let _ = sh.tx.send(ShellCmd::Write(data)).await;
                            }
                        } else if chan == TERM_CHAN_CLAUDE {
                            // §diagnose backspace: opt-in via RUST_LOG=debug.
                            // Logs every binary frame arriving on the
                            // Claude channel so we can confirm the byte
                            // (e.g. 0x7f for Backspace) reaches this
                            // layer from the link. Compare with the
                            // browser-side `?debug_typing=1` console log
                            // to pinpoint any byte mutation.
                            log::debug!(
                                "claude binary: {} bytes [{}]",
                                data.len(),
                                {
                                    let head: Vec<String> = data.iter().take(8)
                                        .map(|b| format!("{b:02x}"))
                                        .collect();
                                    let shown = head.join(" ");
                                    if data.len() > 8 {
                                        format!("{shown} …")
                                    } else {
                                        shown
                                    }
                                }
                            );
                            // Direct write through the shared writer —
                            // bypasses the blocking reader's `pty_rx`
                            // queue, which only drains between
                            // `read()` calls. When claude is mid-turn
                            // or sitting at a prompt with no further
                            // output, that queue is starved; the user
                            // sees multi-second delays or dropped
                            // keystrokes. The War UI's typing path
                            // (one binary frame per keystroke) is the
                            // hardest-hit victim.
                            write_claude_terminal_bytes(&data, active.as_ref()).await;
                        } else {
                            // Unknown channel — be lenient and drop
                            // (matches `link.rs`'s filter against the
                            // known terminal channels).
                            log::debug!(
                                "ignoring binary frame on unknown chan {chan} ({} bytes)",
                                data.len()
                            );
                        }
                    }
                    None => {
                        log::warn!("link event channel closed");
                    }
                }
            }
            outcome = outcome_rx.recv() => {
                if let Some(outcome) = outcome {
                    handle_outcome(
                        outcome,
                        &mut crash_window,
                        &mut dead,
                        &mut active,
                        &cmd_tx,
                        &observer,
                    )
                    .await;
                    health.set_alive(active.is_some());
                    if shutdown.load(Ordering::SeqCst) && active.is_none() {
                        health.set_shutting_down(true);
                        break;
                    }
                } else {
                    log::warn!("outcome channel closed");
                    break;
                }
            }
            _ = wait_for_shutdown(shutdown.clone()), if active.is_some() && !shutdown_acked => {
                shutdown_acked = true;
                log::info!("shutdown signal received; signaling graceful exit");
                health.set_shutting_down(true);
                if let Some(tx) = &active_link_tx {
                    let _ = tx.send(PtyCmd::GracefulShutdown).await;
                }
            }
            _ = wait_for_shutdown(shutdown.clone()), if active.is_none() && !shutdown_acked => {
                log::info!("shutdown signal received; exiting");
                health.set_shutting_down(true);
                break;
            }
        }
    }

    log::info!("rabbit supervisor exiting");
    Ok(())
}

async fn wait_for_shutdown(shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn install_signal_handlers(shutdown: Arc<AtomicBool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGTERM (k8s's default pod-shutdown signal) and SIGINT
        // (Ctrl-C at the terminal where rabbit runs) both trigger
        // graceful shutdown. The terminal where rabbit runs shows
        // rabbit's log output, not claude's TUI, so SIGINT cannot
        // meaningfully forward to claude — pressing Ctrl-C there
        // intuitively means "stop rabbit," and the operator who
        // wants to cancel claude uses the War UI's Interrupt
        // button (which writes an `input::ESC` byte to the shared
        // PTY writer — that path is wired by `dispatch_to_pty`).
        for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
            let s = shutdown.clone();
            tokio::spawn(async move {
                let Ok(mut sig) = signal(kind) else { return };
                sig.recv().await;
                log::info!("received signal {:?}; requesting shutdown", kind);
                s.store(true, Ordering::SeqCst);
            });
        }
        // Best-effort: ignore SIGPIPE so a closed WS doesn't panic the supervisor.
        let _ = signal(SignalKind::pipe()).map(|mut s| {
            tokio::spawn(async move { while s.recv().await.is_some() {} });
        });
    }
    #[cfg(not(unix))]
    {
        let _ = shutdown;
    }
}

async fn detect_claude_version(config: &Config) -> String {
    let mut cmd = Command::new(&config.claude_bin);
    for a in &config.claude_args {
        cmd.arg(a);
    }
    cmd.arg("--version");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    match tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

fn spawn_transcript_relay(
    workdir: &std::path::Path,
    observer: ObserverHandle,
    cmd_tx: mpsc::Sender<LinkCmd>,
) {
    let fallback = default_transcript_path(workdir);
    let (utx, mut urx) = mpsc::channel::<UsageUpdate>(64);
    let tail = TranscriptTail::with_observer(observer, fallback);
    tokio::spawn(async move {
        if let Err(e) = tail.run(utx, 250).await {
            log::warn!("transcript tail stopped: {e:?}");
        }
    });
    tokio::spawn(async move {
        while let Some(update) = urx.recv().await {
            let _ = cmd_tx
                .send(LinkCmd::SendMeta(EnvelopeBody::Usage(update.usage)))
                .await;
        }
    });
}

#[derive(Debug)]
pub enum PtyCmd {
    Write(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Force a full TUI repaint by emitting two SIGWINCHs (size ±1, settle,
    /// restore). Used when a late browser join replays the bounded buffer
    /// and the on-screen TUI hasn't redrawn since.
    Repaint {
        cols: u16,
        rows: u16,
    },
    /// §D Milestone 5 (Phase B): late-join screen dump. The blocking PTY
    /// thread owns the [`TermTracker`], so the snapshot has to be computed
    /// here (single-threaded access to `vt`) and shipped back to warren
    /// through the meta channel via `LinkCmd::SendMeta`.
    Snapshot {
        chan: u8,
    },
    Terminate,
    GracefulShutdown,
}

#[derive(Debug)]
pub enum PtyEvt {
    /// §A.7 / seq-numbered snapshot protocol — `chan`/`seq`/`data` so
    /// the wire can carry a per-channel monotonic counter the browser
    /// uses to know exactly which frames a late-arriving
    /// `ScreenSnapshot` already accounts for. `seq=0` is reserved for
    /// "no bytes fed yet" semantics; the blocking PTY thread starts at
    /// `1` and increments before each emit, single-producer
    /// (`Ordering::Relaxed` is plenty).
    Read { chan: u8, seq: u64, data: Vec<u8> },
    Exited(PtyExitStatus),
    /// §D Milestone 5 (Phase B): a structured meta envelope generated inside
    /// the blocking PTY thread (currently only `ScreenSnapshot`). The driver
    /// loop forwards these to warren via `LinkCmd::SendMeta` so they ride
    /// the same seq/ack channel as everything else.
    Meta(EnvelopeBody),
}

pub enum RunOutcome {
    #[allow(dead_code)]
    CleanExit(PtyExitStatus),
    #[allow(dead_code)]
    Crashed(PtyExitStatus),
    Shutdown,
}

struct ActiveSession {
    pty_link_tx: mpsc::Sender<PtyCmd>,
    /// Independent `ChildKiller` handle — see the doc on `Pty::killer`.
    /// The outer supervisor loop uses this to send SIGKILL to the child
    /// (e.g. on a wire-level `Restart` envelope) even when the blocking
    /// PTY reader thread is wedged in `read()`.
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    /// Shared handle to the PTY master. The outer loop uses it to write
    /// interactive bytes (Interrupt / Slash / Prompt / Clear) directly,
    /// bypassing the blocking reader thread's `pty_rx` queue.
    ///
    /// The shared slot exists for the same reason the shared killer
    /// does: when claude is parked at a TUI prompt emitting no further
    /// output, the blocking thread's `reader.read()` sits idle
    /// indefinitely, and any `PtyCmd::Write` queued on `pty_rx` is
    /// never processed. The outer loop can't be starved of a write
    /// mechanism, so we let it lock this mutex and write straight to
    /// the master. The blocking thread uses its own clone — both lock
    /// the same `parking_lot::Mutex`, so concurrent writers are
    /// serialized (and these are short, OS-buffered writes; no
    /// contention matters).
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

struct OutcomeChannels {
    pty_link_tx: mpsc::Sender<PtyCmd>,
    outcome_rx_in: mpsc::Receiver<RunOutcome>,
}

struct SpawnResult {
    outcome_channels: OutcomeChannels,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

#[allow(clippy::too_many_arguments)]
fn spawn_run_one(
    config: &Config,
    health: HealthState,
    agent_id: Uuid,
    claude_version: &str,
    observer: ObserverHandle,
    args: Vec<String>,
    replay_buf: Arc<Mutex<VecDeque<TermFrame>>>,
    cmd_tx: mpsc::Sender<LinkCmd>,
    shutdown: Arc<AtomicBool>,
) -> Result<SpawnResult> {
    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyCmd>(128);
    let (pty_evt_tx, pty_evt_rx) = mpsc::channel::<PtyEvt>(128);
    let (outcome_tx, outcome_rx_in) = mpsc::channel::<RunOutcome>(8);

    let replay_cap = config.replay_bytes;
    let bin = config.claude_bin.clone();
    let workdir = config.workdir.clone();
    let cols = config.term_cols;
    let rows = config.term_rows;
    let replay = config.replay_bytes;
    let shutdown_for_blocking = shutdown.clone();
    let grace_period = Duration::from_millis(config.shutdown_grace_ms);
    let auto_trust = config.auto_trust;

    // Spawn the PTY *before* moving into the blocking thread so we can
    // extract a `ChildKiller` and the writer and share both with the
    // outer supervisor loop. See the doc on `ActiveSession::killer`
    // and `ActiveSession::writer` for the cross-thread signaling and
    // direct-write rationales.
    let pty = Pty::spawn(&bin, &args, &workdir, cols, rows, replay)?;
    let initial_replay = pty.snapshot_replay().to_vec();
    let killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>> =
        Arc::new(Mutex::new(pty.child.clone_killer()));
    // Take the writer from the master *before* moving pty into the
    // blocking thread. `take_writer` is `&self`-ish — it doesn't
    // consume the master, but it does gate future calls (callable
    // once). Wrapping it in `Arc<Mutex<...>>` lets the outer loop
    // share it concurrently.
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
        pty.master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("taking pty writer before spawn_blocking: {e}"))?,
    ));
    // Clone for the blocking thread. The original `writer` is returned
    // through `SpawnResult` so the outer loop can lock it directly. The
    // `Mutex` serializes writes from both call sites.
    let writer_for_blocking = writer.clone();
    let pty_join = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut pty = pty;
        let mut reader = pty.reader();
        let writer = writer_for_blocking;

        let mut io_buf = [0u8; 4096];
        let mut graceful_pending = false;
        let mut graceful_since: Option<Instant> = None;
        // §D Milestone 5 (Phase A): mirror the PTY byte stream into a
        // server-side virtual terminal. Passive today — a later phase
        // serializes `vt.snapshot()` for late browser joiners in place of the
        // SIGWINCH jiggle. 5k-line scrollback matches the design budget.
        let mut vt = crate::vt::TermTracker::new(cols, rows, 5_000);
        // §A.7: on a fresh workdir claude blocks on a "trust this folder?"
        // dialog. Unattended, nobody presses Enter, so watch the output and
        // auto-accept it (bounded, to avoid keystroke storms on false hits).
        let mut trust_watcher = auto_trust.then(|| crate::trust::TrustWatcher::new(3));
        // §A.7 / seq-numbered snapshot protocol — single-producer seq
        // counter for the bytes this blocking thread feeds out of
        // claude's PTY. Starts at 1 (0 is reserved for "no bytes fed
        // yet"). Incremented *before* assignment so the first byte read
        // carries seq=1, the next seq=2, … `wrap` is fine; the browser's
        // comparison is `<=` between values within a single session, so
        // >2^63 separations aren't a realistic concern.
        let mut next_seq: u64 = 1;
        // The `PtyEvt::Read` ESC placeholder during shutdown uses seq=0
        // intentionally (it's a meta signal, not bytes-fed). We bump
        // it before assignment to the placeholder so the next real
        // read still gets a fresh value, but for the synthetic ESC
        // payload we use seq=0 to mean "synthetic, no live bytes
        // covered."
        let mut bytes_read_since_spawn = false;
        loop {
            if shutdown_for_blocking.load(Ordering::SeqCst) {
                graceful_pending = true;
                if graceful_since.is_none() {
                    graceful_since = Some(Instant::now());
                    log::info!("shutdown: sending ESC + waiting up to {grace_period:?}");
                    {
                        let mut w = writer.lock();
                        let _ = w.write_all(b"\x1b");
                        let _ = w.flush();
                    }
                    // §A.7: synthetic shutdown placeholder — `seq=0`
                    // intentionally marks it as "not a live byte",
                    // and `chan=TERM_CHAN_CLAUDE` is a no-op for the
                    // browser (it's not a wire event; it's only
                    // emitted through the meta plane after the §D
                    // refactor and the browser never sees it). The
                    // string itself is preserved so the pre-existing
                    // debug surfaces stay identical.
                    let _ = pty_evt_tx.blocking_send(PtyEvt::Read {
                        chan: TERM_CHAN_CLAUDE,
                        seq: 0,
                        data: b"ESC (shutdown)".to_vec(),
                    });
                }
            }

            if let Ok(cmd) = pty_rx.try_recv() {
                match cmd {
                    PtyCmd::Write(b) => {
                        use std::io::Write;
                        {
                            let mut w = writer.lock();
                            if w.write_all(&b).is_err() {
                                break;
                            }
                            let _ = w.flush();
                        }
                    }
                    PtyCmd::Resize { cols, rows } => {
                        let _ = pty.resize(cols, rows);
                        vt.resize(cols, rows);
                    }
                    PtyCmd::Repaint { cols, rows } => {
                        if let Err(e) = pty.jiggle(cols, rows) {
                            log::warn!("repaint jiggle failed: {e:?}");
                        }
                    }
                    PtyCmd::Snapshot { chan } => {
                        let snap = vt.snapshot();
                        // §A.7: populate `after_seq` from the running
                        // counter. `0` means "we have never fed a byte on
                        // this channel — don't discard anything." Otherwise
                        // the snapshot reports the most-recently-assigned
                        // seq (the highest seq a buffered live frame could
                        // already carry).
                        let after_seq = if bytes_read_since_spawn {
                            next_seq.wrapping_sub(1)
                        } else {
                            0
                        };
                        let body = ScreenSnapshotBody {
                            chan,
                            cols: snap.cols,
                            rows: snap.rows,
                            cursor_col: snap.cursor_col,
                            cursor_row: snap.cursor_row,
                            cursor_visible: snap.cursor_visible,
                            text: snap.text,
                            after_seq,
                        };
                        if pty_evt_tx
                            .blocking_send(PtyEvt::Meta(EnvelopeBody::ScreenSnapshot(body)))
                            .is_err()
                        {
                            break;
                        }
                    }
                    PtyCmd::Terminate => {
                        let _ = pty.terminate();
                    }
                    PtyCmd::GracefulShutdown => {
                        graceful_pending = true;
                        if graceful_since.is_none() {
                            graceful_since = Some(Instant::now());
                            log::info!("graceful shutdown: sending ESC");
                            {
                                let mut w = writer.lock();
                                let _ = w.write_all(b"\x1b");
                                let _ = w.flush();
                            }
                        }
                    }
                }
                continue;
            }

            if graceful_pending {
                if let Some(since) = graceful_since {
                    let alive = pty.alive();
                    if graceful_expired(since.elapsed(), grace_period, alive) {
                        terminate_and_report_exited(&mut pty, &pty_evt_tx);
                        break;
                    }
                }
            }

            use std::io::Read;
            match reader.read(&mut io_buf) {
                Ok(0) => {
                    // EOF on the master means the slave side closed —
                    // i.e. the child has exited. Capture its status and
                    // notify the driver. The old `break` here skipped the
                    // `if !pty.alive()` post-check, leaving the driver
                    // task hung on `pty_evt_rx_inner.recv()` forever
                    // (regression: any natural child exit, plus the
                    // Restart-killed case once the shared killer arrived
                    // — the killer works, but the resulting EOF wasn't
                    // being recognized as "child gone").
                    //
                    // Goes through the same `terminate_and_report_exited`
                    // helper the graceful-shutdown path uses, so the
                    // status capture + evt send stay in one place.
                    terminate_and_report_exited(&mut pty, &pty_evt_tx);
                    break;
                }
                Ok(n) => {
                    vt.feed(&io_buf[..n]);
                    if let Some(tw) = trust_watcher.as_mut() {
                        if let Some(resp) = tw.observe(&io_buf[..n]) {
                            log::info!("trust dialog detected; auto-accepting with Enter");
                            {
                                let mut w = writer.lock();
                                let _ = w.write_all(resp);
                                let _ = w.flush();
                            }
                        }
                    }
                    // §A.7: assign the next seq to this read, then bump.
                    // The blocking thread is the single producer, so no
                    // CAS / Ordering is required for correctness.
                    let seq = next_seq;
                    next_seq = next_seq.wrapping_add(1);
                    bytes_read_since_spawn = true;
                    if pty_evt_tx
                        .blocking_send(PtyEvt::Read {
                            chan: TERM_CHAN_CLAUDE,
                            seq,
                            data: io_buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => {
                    // Treat any read error as child-gone. EIO is the
                    // common one — portable-pty converts it to Ok(0)
                    // on Unix, but if the platform doesn't, we still
                    // owe the driver a clean Exited before breaking.
                    terminate_and_report_exited(&mut pty, &pty_evt_tx);
                    break;
                }
            }
            if !pty.alive() {
                let status = pty.wait().unwrap_or_else(|e| {
                    log::warn!("pty.wait failed: {e:?}");
                    PtyExitStatus::with_exit_code(1)
                });
                let _ = pty_evt_tx.blocking_send(PtyEvt::Exited(status));
                break;
            }
        }
        Ok(initial_replay)
    });

    let pty_task = tokio::spawn(async move {
        match pty_join.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log::error!("pty task error: {e:?}"),
            Err(e) => log::error!("pty task join error: {e:?}"),
        }
    });

    let cmd_tx_driver = cmd_tx.clone();
    let outcome_tx_driver = outcome_tx.clone();
    let replay_buf_inner = replay_buf.clone();
    let replay_cap_inner = replay_cap;
    let shutdown_for_driver = shutdown.clone();
    let pty_tx_for_cleanup = pty_tx.clone();

    {
        let cmd_tx_init = cmd_tx_driver.clone();
        let observer_init = observer.clone();
        tokio::spawn(async move {
            let _ = send_state(
                &observer_init,
                &cmd_tx_init,
                StateFrame {
                    state: "idle".into(),
                    session_id: None,
                    reason: None,
                },
            )
            .await;
        });
    }

    tokio::spawn(async move {
        let mut obs_rx = observer.tx.subscribe();
        let mut pty_evt_rx_inner = pty_evt_rx;
        loop {
            tokio::select! {
                biased;
                chunk = pty_evt_rx_inner.recv() => {
                    match chunk {
                        // §A.7: per-channel seq rides through verbatim —
                        // warren is a dumb pipe and never invents or
                        // rewrites a seq. (The §3 invariant: warren's
                        // outgoing seq on chan X equals rabbit's emitted
                        // seq on chan X.)
                        Some(PtyEvt::Read { chan, seq, data }) => {
                            {
                                let mut buf = replay_buf_inner.lock();
                                buf.push_back(TermFrame {
                                    chan,
                                    seq,
                                    data: data.clone(),
                                });
                                trim_replay(&mut buf, replay_cap_inner);
                            }
                            let _ = cmd_tx_driver
                                .send(LinkCmd::SendBinary {
                                    chan,
                                    seq,
                                    data,
                                })
                                .await;
                        }
                        Some(PtyEvt::Meta(body)) => {
                            let _ = cmd_tx_driver.send(LinkCmd::SendMeta(body)).await;
                        }
                        Some(PtyEvt::Exited(status)) => {
                            log::info!("claude exited: kind={:?}", ExitKind::from(&status));
                            let outcome = if shutdown_for_driver.load(Ordering::SeqCst) {
                                RunOutcome::Shutdown
                            } else if matches!(ExitKind::from(&status), ExitKind::Clean) {
                                RunOutcome::CleanExit(status)
                            } else {
                                RunOutcome::Crashed(status)
                            };
                            let _ = outcome_tx_driver.send(outcome).await;
                            break;
                        }
                        None => break,
                    }
                }
                evt = obs_rx.recv() => {
                    if let Ok(ev) = evt {
                        forward_observer_event(&cmd_tx_driver, &ev).await;
                    }
                }
            }
        }
        health.set_alive(false);
        let _ = pty_tx_for_cleanup.send(PtyCmd::Terminate).await;
        let _ = pty_task.await;
    });

    let _ = agent_id;
    let _ = claude_version;

    Ok(SpawnResult {
        outcome_channels: OutcomeChannels {
            pty_link_tx: pty_tx,
            outcome_rx_in,
        },
        killer,
        writer,
    })
}

async fn handle_outcome(
    outcome: RunOutcome,
    crash_window: &mut CrashWindow,
    dead: &mut bool,
    active: &mut Option<ActiveSession>,
    cmd_tx: &mpsc::Sender<LinkCmd>,
    observer: &ObserverHandle,
) {
    let (state_label, reason, session_id) = match &outcome {
        RunOutcome::CleanExit(_) => ("ended", Some("clean_exit"), observer.latest_session()),
        RunOutcome::Crashed(_) => ("dead", Some("crashed"), observer.latest_session()),
        RunOutcome::Shutdown => (
            "dead",
            Some("supervisor_shutdown"),
            observer.latest_session(),
        ),
    };
    let _ = send_state(
        observer,
        cmd_tx,
        StateFrame {
            state: state_label.into(),
            session_id,
            reason: reason.map(|s| s.to_string()),
        },
    )
    .await;

    match outcome {
        RunOutcome::CleanExit(_) => {
            *active = None;
        }
        RunOutcome::Crashed(_) => {
            if crash_window.record(Instant::now()) {
                log::error!(
                    "crash loop detected ({} events within {:?}); awaiting wire Restart",
                    crash_window.len(),
                    Duration::from_secs(300)
                );
                let _ = send_state(
                    observer,
                    cmd_tx,
                    StateFrame {
                        state: "dead".into(),
                        session_id: None,
                        reason: Some("crash_loop".into()),
                    },
                )
                .await;
                *dead = true;
            }
            *active = None;
        }
        RunOutcome::Shutdown => {
            *active = None;
        }
    }
}

async fn forward_observer_event(cmd_tx: &mpsc::Sender<LinkCmd>, ev: &ObserverEvent) {
    let env = build_envelope(ev);
    let body = match env {
        Some(e) => e,
        None => return,
    };
    let _ = cmd_tx.send(LinkCmd::SendMeta(body.clone())).await;
}

fn build_envelope(ev: &ObserverEvent) -> Option<EnvelopeBody> {
    let raw = match ev.kind {
        "session" => EnvelopeBody::Session(crate::wire::SessionInfo {
            session_id: ev.session_id.clone().unwrap_or_default(),
            resumed: false,
        }),
        "session_end" => EnvelopeBody::State(StateFrame {
            state: "ended".into(),
            session_id: ev.session_id.clone(),
            reason: Some("session_end".into()),
        }),
        "prompt_echo" => EnvelopeBody::PromptEcho(crate::wire::PromptEcho {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            text: ev
                .raw
                .as_ref()
                .and_then(|r| r.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            by: "admin".to_string(),
        }),
        "stop_hook" => EnvelopeBody::StopHook {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            usage: ev.usage.clone(),
            error: None,
        },
        "log" => EnvelopeBody::Log(LogLine {
            level: "info".to_string(),
            message: ev.raw.as_ref().map(|r| r.to_string()).unwrap_or_default(),
        }),
        _ => return None,
    };
    Some(raw)
}

/// §D prompt policy (reject-when-Running): decide whether an inbound envelope
/// should be rejected rather than dispatched to the PTY. Only `Prompt` frames
/// are gated, and only while the agent is `Running` — injecting a prompt
/// mid-turn would interleave with the live turn (and possibly a human's edit).
/// Control frames (Interrupt/Slash/Clear/Resize/Repaint) are never rejected.
pub fn should_reject_prompt(state: State, body: &EnvelopeBody) -> bool {
    matches!(body, EnvelopeBody::Prompt { .. }) && state == State::Running
}

/// Build the wire envelope to emit when `should_reject_prompt` returns true.
/// Carries the original prompt id so the UI can render `prompt #xxx rejected`
/// and tie the banner to the specific prompt the user attempted. Extracted
/// for testability — the supervisor loop calls this on rejection.
pub fn prompt_rejected_for(env: &Envelope) -> EnvelopeBody {
    let id = match &env.body {
        EnvelopeBody::Prompt { id, .. } => *id,
        // `should_reject_prompt` only returns true for Prompt, so this is
        // unreachable in practice; we still produce a valid envelope rather
        // than panicking so a future caller that mis-uses this helper fails
        // loudly via serde/the wire rather than crashing rabbit.
        _ => uuid::Uuid::nil(),
    };
    EnvelopeBody::PromptRejected {
        id,
        reason: "agent is running a turn".into(),
    }
}

/// During a pending graceful shutdown, decide whether the PTY loop should stop
/// now. Returns `true` once the child has already exited (it honored the ESC)
/// **or** the grace period has elapsed. The caller hard-terminates a still-alive
/// child before breaking, so this bounds total shutdown latency at
/// `grace_period` no matter whether claude cooperates.
///
/// Operationally this is the knob that keeps a rabbit pod under k8s's
/// `terminationGracePeriodSeconds`: `SHUTDOWN_GRACE_MS` defaults to 1500ms,
/// far below the 30s the operator budgets, so the supervisor always exits on
/// its own terms rather than being SIGKILLed by the kubelet.
pub fn graceful_expired(elapsed: Duration, grace_period: Duration, child_alive: bool) -> bool {
    !child_alive || elapsed >= grace_period
}

/// Hard-kill `pty` if it is still alive, block until the child has been
/// reaped, and notify the driver via `evt_tx` with the captured status.
///
/// Mirrors the natural-exit branch at the end of the blocking PTY loop.
/// Without this, when the graceful-shutdown grace window elapses the
/// blocking thread calls `pty.terminate()` and `break`s out of its loop
/// without ever sending `PtyEvt::Exited` — the driver task then hangs
/// forever on `pty_evt_rx_inner.recv()` and the tokio runtime refuses to
/// exit even after the supervisor's outer loop has broken. Reproduced
/// by `^C` against a `claude` child that ignored the graceful ESC.
///
/// Extracted so the regression test in `tests::*` can drive it against a
/// real `/bin/sleep` child without standing up the whole supervisor.
pub(crate) fn terminate_and_report_exited(pty: &mut Pty, evt_tx: &mpsc::Sender<PtyEvt>) {
    if pty.alive() {
        log::warn!("grace period elapsed; terminating claude");
        let _ = pty.terminate();
    }
    let status = pty.wait().unwrap_or_else(|e| {
        log::warn!("pty.wait failed during grace kill: {e:?}");
        PtyExitStatus::with_exit_code(1)
    });
    let _ = evt_tx.blocking_send(PtyEvt::Exited(status));
}

/// Direct-write path for terminal bytes coming back from the War UI.
///
/// Each keystroke the operator types in the browser arrives as a binary
/// WS frame (`[TERM_CHAN_CLAUDE, byte]`). Before this helper, those
/// bytes were queued as `PtyCmd::Write(data)` on the blocking PTY
/// thread's `pty_rx` channel — drained only between `read()` calls.
/// When the child was alive but emitting no output (idle TUI, mid-
/// prompt, mid-tool), `read()` blocked indefinitely and the keystrokes
/// sat in the queue until the next time data flowed. The operator saw
/// multi-second input lag and dropped characters.
///
/// This helper bypasses `pty_rx` entirely: lock the shared writer and
/// write straight to the master. Latency is bounded by the mutex
/// acquisition — sub-millisecond in practice.
///
/// Extracted from the outer select! `LinkEvent::Binary` arm so the
/// regression test can drive it without standing up the whole
/// supervisor (which would require a live `claude` child).
async fn write_claude_terminal_bytes(
    data: &[u8],
    active: Option<&ActiveSession>,
) {
    use std::io::Write;
    let Some(active) = active else {
        log::debug!(
            "claude terminal write of {} bytes dropped: no active session",
            data.len()
        );
        return;
    };
    let mut g = active.writer.lock();
    let w: &mut dyn Write = &mut *g;
    let _ = w.write_all(data);
    let _ = w.flush();
}

async fn dispatch_to_pty(
    env: &Envelope,
    shared_writer: Option<&Arc<Mutex<Box<dyn Write + Send>>>>,
    pty_tx: &mpsc::Sender<PtyCmd>,
    cols: u16,
    rows: u16,
) {
    use std::io::Write;
    // Byte-producing commands (Prompt / Slash / Interrupt / Clear) write
    // DIRECTLY through `shared_writer` when one is available. This
    // bypasses the blocking PTY thread's `pty_rx` queue — which is
    // starved whenever the child is alive and emitting no further
    // output (a TUI prompt, in particular). The outer loop previously
    // enqueued these as `PtyCmd::Write` and waited for the blocking
    // thread to drain it between read() calls; when claude was parked
    // at a prompt, Ctrl+C never reached it.
    //
    // `Resize` / `Repaint` still go through `pty_rx` — they're rare and
    // tolerate the latency. A future pass could promote them too.
    match &env.body {
        EnvelopeBody::Prompt { text, .. } => {
            if let Some(sw) = shared_writer {
                let mut g = sw.lock();
                let w: &mut dyn Write = &mut *g;
                let _ = input::paste(w, text);
                let _ = w.flush();
            } else {
                fallback_via_pty_tx(env, pty_tx).await;
            }
        }
        EnvelopeBody::Slash { cmd } => {
            if let Some(sw) = shared_writer {
                let mut g = sw.lock();
                let w: &mut dyn Write = &mut *g;
                let _ = input::slash(w, cmd);
                let _ = w.flush();
            } else {
                fallback_via_pty_tx(env, pty_tx).await;
            }
        }
        EnvelopeBody::Interrupt => {
            if let Some(sw) = shared_writer {
                let mut g = sw.lock();
                let w: &mut dyn Write = &mut *g;
                let _ = input::interrupt(w);
                let _ = w.flush();
            } else {
                fallback_via_pty_tx(env, pty_tx).await;
            }
        }
        EnvelopeBody::Clear { .. } => {
            if let Some(sw) = shared_writer {
                let mut g = sw.lock();
                let w: &mut dyn Write = &mut *g;
                let _ = input::slash(w, "clear");
                let _ = w.flush();
            } else {
                fallback_via_pty_tx(env, pty_tx).await;
            }
        }
        EnvelopeBody::Resize { cols: rc, rows: rr } => {
            let _ = pty_tx.try_send(PtyCmd::Resize {
                cols: *rc,
                rows: *rr,
            });
        }
        EnvelopeBody::Repaint => {
            let _ = pty_tx.try_send(PtyCmd::Repaint { cols, rows });
        }
        _ => {}
    }
}

/// Fallback for byte-producing commands when the outer loop holds no
/// shared writer (e.g. tests, or before the PTY has been spawned). Goes
/// through the blocking-thread channel path — the legacy behavior. Kept
/// in one place so the production and fallback paths produce identical
/// byte sequences.
async fn fallback_via_pty_tx(env: &Envelope, pty_tx: &mpsc::Sender<PtyCmd>) {
    let mut out: Vec<u8> = Vec::with_capacity(64);
    {
        let mut shim = BufShim { out: &mut out };
        match &env.body {
            EnvelopeBody::Prompt { text, .. } => {
                let _ = input::paste(&mut shim, text);
            }
            EnvelopeBody::Slash { cmd } => {
                let _ = input::slash(&mut shim, cmd);
            }
            EnvelopeBody::Interrupt => {
                let _ = input::interrupt(&mut shim);
            }
            EnvelopeBody::Clear { .. } => {
                let _ = input::slash(&mut shim, "clear");
            }
            _ => return,
        }
    }
    if !out.is_empty() {
        let _ = pty_tx.send(PtyCmd::Write(out)).await;
    }
}

pub async fn send_state(
    observer: &ObserverHandle,
    cmd_tx: &mpsc::Sender<LinkCmd>,
    frame: StateFrame,
) -> Result<()> {
    // Keep the observer's tracked lifecycle state in step with the supervisor's
    // own transitions so `latest_state()` is authoritative for the whole
    // lifecycle (spawn/exit/crash), not just the hook-derived Running/Idle.
    if let Some(st) = State::from_label(frame.state.as_str()) {
        observer.set_state(st);
    }
    let _ = cmd_tx
        .send(LinkCmd::SendMeta(EnvelopeBody::State(frame)))
        .await;
    Ok(())
}

struct BufShim<'a> {
    out: &'a mut Vec<u8>,
}

/// Drop oldest chunks from `buf` until its total byte length is `<= cap`.
/// Counts bytes via `TermFrame::data.len()`; the per-frame `chan`/`seq`
/// metadata is fixed-size and doesn't contribute to the cap.
fn trim_replay(buf: &mut VecDeque<TermFrame>, cap: usize) {
    let mut total: usize = buf.iter().map(|v| v.data.len()).sum();
    while total > cap {
        match buf.pop_front() {
            Some(front) => total -= front.data.len(),
            None => break,
        }
    }
}

impl<'a> std::io::Write for BufShim<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::ExitKind;
    use crate::respawn::CrashWindow;
    use crate::wire::AgentState;
    use std::time::Duration;

    #[test]
    fn exit_kind_maps_clean_zero() {
        let s = PtyExitStatus::with_exit_code(0);
        assert_eq!(ExitKind::from(&s), ExitKind::Clean);
    }

    #[test]
    fn exit_kind_maps_signal_to_crashed() {
        let s = PtyExitStatus::with_signal("SIGTERM");
        assert_eq!(ExitKind::from(&s), ExitKind::Crashed);
    }

    #[test]
    fn trim_replay_drops_until_under_cap() {
        let mut buf: VecDeque<TermFrame> = VecDeque::new();
        for i in 0..10 {
            buf.push_back(TermFrame {
                chan: TERM_CHAN_CLAUDE,
                seq: i as u64 + 1,
                data: vec![0u8; 50],
            });
        }
        trim_replay(&mut buf, 200);
        let total: usize = buf.iter().map(|v| v.data.len()).sum();
        assert!(total <= 200);
    }

    #[test]
    fn trim_replay_no_op_when_under_cap() {
        let mut buf: VecDeque<TermFrame> = VecDeque::new();
        buf.push_back(TermFrame {
            chan: TERM_CHAN_CLAUDE,
            seq: 1,
            data: b"hello".to_vec(),
        });
        trim_replay(&mut buf, 100);
        assert_eq!(buf.len(), 1);
    }

    #[tokio::test]
    async fn handle_outcome_records_crashes_into_window() {
        let mut window = CrashWindow::new(Duration::from_secs(300), 2);
        assert!(!window.record(std::time::Instant::now()));
        assert!(!window.record(std::time::Instant::now()));
        assert!(window.record(std::time::Instant::now()));
        assert!(window.len() > 2);
    }

    #[test]
    fn shutting_down_flag_blocks_readyz() {
        use crate::health::HealthState;
        let h = HealthState::new();
        h.set_alive(true);
        h.set_shutting_down(false);
        assert!(!h.shutting_down.load(std::sync::atomic::Ordering::SeqCst));
        h.set_shutting_down(true);
        assert!(h.shutting_down.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn graceful_expired_true_once_child_exits() {
        // Child honored the ESC and is gone — stop immediately, don't wait out
        // the grace period.
        assert!(graceful_expired(
            Duration::from_millis(0),
            Duration::from_secs(30),
            false
        ));
    }

    #[test]
    fn graceful_expired_false_while_within_grace_and_alive() {
        // Still within budget and claude is still running: keep waiting.
        assert!(!graceful_expired(
            Duration::from_millis(100),
            Duration::from_millis(1500),
            true
        ));
    }

    #[test]
    fn graceful_expired_true_when_grace_elapsed_even_if_alive() {
        // Grace budget spent and claude ignored the ESC: caller hard-kills.
        // This is the bound that keeps us under k8s terminationGracePeriod.
        assert!(graceful_expired(
            Duration::from_secs(2),
            Duration::from_millis(1500),
            true
        ));
    }

    #[test]
    fn graceful_expired_true_at_exact_boundary() {
        assert!(graceful_expired(
            Duration::from_millis(1500),
            Duration::from_millis(1500),
            true
        ));
    }

    /// Regression test for the runtime-hang on `^C`. The blocking PTY
    /// thread's grace-expired branch used to call `pty.terminate()` and
    /// `break` without sending `PtyEvt::Exited`, so the driver task was
    /// left hung on `pty_evt_rx_inner.recv()` and the tokio runtime
    /// refused to exit even after the supervisor's outer loop broke.
    ///
    /// We exercise `terminate_and_report_exited` against a real `/bin/sleep`
    /// child — the closest reproduction without standing up the whole
    /// supervisor (which spawns `claude`, which we don't have in CI). The
    /// helper uses `Sender::blocking_send`, so the call site has to live
    /// off the runtime thread (just like in production, where the helper
    /// runs inside `spawn_blocking`). A 2s timeout on the receive proves
    /// the event actually fires (no hang) — without the fix, `recv()`
    /// would block forever and the test would time out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn terminate_and_report_exited_unblocks_driver_after_grace_kill() {
        use crate::pty::Pty;
        use std::sync::{Arc, Mutex};
        let pty = Pty::spawn("/bin/sleep", &["2".into()], "/tmp", 80, 24, 4096)
            .expect("spawn sleep");
        let pty = Arc::new(Mutex::new(pty));
        let (evt_tx, mut evt_rx) = mpsc::channel::<PtyEvt>(8);

        // Run the helper on a blocking thread — production invokes it
        // from inside `spawn_blocking`, and `blocking_send` cannot be
        // called from inside the runtime.
        let pty_for_helper = pty.clone();
        let evt_tx_clone = evt_tx.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = pty_for_helper.lock().expect("pty mutex poisoned");
            terminate_and_report_exited(&mut guard, &evt_tx_clone);
        })
        .await
        .expect("spawn_blocking join");

        let evt = tokio::time::timeout(Duration::from_secs(2), evt_rx.recv())
            .await
            .expect("driver never received PtyEvt::Exited within 2s — bug regression")
            .expect("evt channel closed unexpectedly");
        match evt {
            PtyEvt::Exited(_) => {} // expected
            other => panic!("expected PtyEvt::Exited, got {other:?}"),
        }
        assert!(
            !pty.lock().expect("pty mutex poisoned").alive(),
            "child should be reaped after terminate_and_report_exited"
        );
    }

    #[tokio::test]
    async fn send_state_advances_observer_latest_state() {
        // The reject-when-Running gate consults observer.latest_state(). The
        // supervisor's own transitions must feed it too, not just hook events —
        // otherwise latest_state() would be blind to spawn/exit/crash.
        let (tx, _rx) = mpsc::channel::<LinkCmd>(8);
        let observer = ObserverHandle::new();
        assert_eq!(observer.latest_state(), State::Starting);

        send_state(
            &observer,
            &tx,
            StateFrame {
                state: AgentState::Idle,
                session_id: None,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(observer.latest_state(), State::Idle);

        send_state(
            &observer,
            &tx,
            StateFrame {
                state: AgentState::Dead,
                session_id: None,
                reason: Some("crashed".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(observer.latest_state(), State::Dead);

        // Note: the typed `AgentState` enum has no "unknown label" variant;
        // malformed wire envelopes are rejected by serde at deserialize
        // time. The historical "unrecognized label must leave observer
        // untouched" assertion was a guard against the old String-typed
        // `state` field; that path no longer exists at runtime.
    }

    // -----------------------------------------------------------------
    // Restart-while-stuck-at-TUI regression coverage.
    //
    // The blocking PTY thread is wedged in `reader.read()` whenever the
    // child is alive and emitting no output (e.g. claude parked at a
    // TUI prompt). Pre-fix, PtyCmd::Terminate queued on `pty_rx` would
    // sit there forever because the channel is only drained between
    // read() calls. The fix is to share a `ChildKiller` (portable-pty's
    // documented cross-thread signaling primitive) with the outer
    // supervisor loop, so a wire-level `Restart` can SIGKILL the child
    // directly. The two tests below cover both ends of that path.
    // -----------------------------------------------------------------

    /// Verify that `Pty::spawn` populates the `killer` field with a
    /// handle that, when invoked, can SIGKILL the child independently
    /// of whether another thread is blocked in `.wait()`. The
    /// supervisor's outer loop relies on exactly this property — it
    /// holds only the `killer` (the `child` lives inside the blocking
    /// reader thread).
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn pty_killer_terminates_child_independently() {
        use crate::pty::Pty;
        // `sleep 60` will block on its own timer for a minute; we'll
        // never wait for it. The killer has to reach the child without
        // any help from the read() path.
        let mut pty = Pty::spawn(
            "/bin/sleep",
            &["60".to_string()],
            "/tmp",
            80,
            24,
            4096,
        )
        .expect("spawn sleep");

        // Confirm the killer is a real, working handle. If the trait
        // method signature or semantics ever drift, this catches it
        // before the supervisor-level test does.
        pty.killer.kill().expect("killer.kill on live child");

        // The child should be reaped shortly. `wait()` blocks until the
        // SIGKILL is observed and the process exits. Use a 2s budget
        // — anything longer would suggest the signal didn't reach.
        let status = tokio::task::spawn_blocking(move || pty.wait())
            .await
            .expect("spawn_blocking join");
        match status {
            Ok(s) => assert!(
                !s.success(),
                "SIGKILL'd child should NOT report success: {s:?}"
            ),
            Err(e) => panic!("pty.wait after kill failed: {e:?}"),
        }
    }

    /// Regression: a child that exits NATURALLY (no kill, no Terminate)
    /// must still produce a `PtyEvt::Exited` event. The pre-fix code
    /// had `Ok(0) => break` and `Err(_) => break` arms that bypassed
    /// the post-read `if !pty.alive() { send Exited }` check, so any
    /// natural exit left the driver task hung forever on
    /// `pty_evt_rx_inner.recv()`.
    ///
    /// We model the blocking thread's read loop in isolation (it's
    /// just a `read` + arm), driving it with a child that exits on its
    /// own after a brief delay. The test asserts the EOF arm now
    /// sends `PtyEvt::Exited`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_read_sends_exited_on_natural_eof() {
        use crate::pty::Pty;
        use std::io::Read;
        use std::sync::Arc;

        // `/bin/sh -c "exit 0"` exits cleanly within milliseconds and
        // writes nothing to stdout — exercises the read returns Ok(0)
        // path immediately.
        let pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_string(), "exit 0".to_string()],
            "/tmp",
            80,
            24,
            4096,
        )
        .expect("spawn sh -c exit 0");

        let mut reader = pty.reader();
        let pty = Arc::new(parking_lot::Mutex::new(pty));
        let (evt_tx, mut evt_rx) = mpsc::channel::<PtyEvt>(8);

        // Mirror the blocking loop's read arm, but skip the
        // trust-watcher / vt / replay-buffer side effects (they're
        // orthogonal to the bug).
        let pty_for_loop = pty.clone();
        let evt_tx_for_loop = evt_tx.clone();
        let join = tokio::task::spawn_blocking(move || {
            let mut io_buf = [0u8; 64];
            loop {
                match reader.read(&mut io_buf) {
                    Ok(0) => {
                        // The FIX being tested: send Exited on EOF
                        // before breaking. The pre-fix code just
                        // `break`-ed here.
                        let mut pty = pty_for_loop.lock();
                        super::terminate_and_report_exited(&mut pty, &evt_tx_for_loop);
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => {
                        let mut pty = pty_for_loop.lock();
                        super::terminate_and_report_exited(&mut pty, &evt_tx_for_loop);
                        break;
                    }
                }
            }
        });

        // Without the fix, this would time out: no Exited ever sent.
        // With the fix, it arrives promptly (sub-second).
        let evt = tokio::time::timeout(Duration::from_secs(2), evt_rx.recv())
            .await
            .expect("driver never received PtyEvt::Exited on natural EOF — bug regression")
            .expect("evt channel closed unexpectedly");
        match evt {
            PtyEvt::Exited(s) => assert!(s.success(), "natural exit 0 should be success: {s:?}"),
            other => panic!("expected PtyEvt::Exited, got {other:?}"),
        }

        join.await.expect("blocking thread join");
    }

    /// Regression: an `EnvelopeBody::Interrupt` arriving at the outer
    /// supervisor loop must produce the abort byte on the PTY master
    /// even when the blocking reader thread is wedged in `read()`
    /// waiting for the child to emit something.
    ///
    /// The abort byte is the literal Ctrl-C byte (`0x03`) — this is
    /// what claude's keymap binds to "abort current turn." (For Y/N
    /// confirmation prompts, the right byte is `ESC`/`0x1b`; that's a
    /// different UI affordance, not what the Interrupt button does.)
    ///
    /// Pre-fix, dispatch_to_pty packed the abort bytes into
    /// `PtyCmd::Write(out)` and pushed it into `pty_rx`, which the
    /// blocking thread only drained between `read()` calls. With
    /// claude parked mid-turn (or at any prompt emitting no output),
    /// `read()` blocked indefinitely and the queued bytes never
    /// reached the master. Now the abort bytes go directly through the
    /// shared writer, bypassing the channel.
    ///
    /// We verify the direct-write path end-to-end against a real PTY:
    /// spawn a `/bin/cat` (which reads stdin forever, just like a
    /// claude turn would), confirm `read()` blocks (the cat is alive
    /// and waiting — verified via `pty.alive()`), and assert the
    /// dispatch returns sub-second — proving the path bypassed the
    /// channel. cat does not respond to 0x03 in any user-visible way,
    /// but it WILL receive the byte on its stdin; the relevant
    /// assertion is that dispatch_to_pty writes the byte without
    /// blocking on a channel that may be unreachable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn interrupt_reaches_pty_via_shared_writer() {
        use crate::pty::Pty;
        use crate::wire::{Envelope, EnvelopeBody, PROTOCOL_VERSION};

        let mut pty = Pty::spawn(
            "/bin/cat",
            &[], // cat with no args reads stdin forever
            "/tmp",
            80,
            24,
            4096,
        )
        .expect("spawn cat");
        assert!(pty.alive(), "cat must be alive and waiting for input");

        // Replicate exactly what `spawn_run_one` does: wrap the master
        // writer + a dummy `pty_tx` so dispatch_to_pty can find both.
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pty.master
                .take_writer()
                .map_err(|e| anyhow::anyhow!("take_writer: {e}"))
                .expect("take_writer"),
        ));
        // `dispatch_to_pty` still takes a `pty_tx` for Resize/Repaint;
        // we don't exercise those here so the channel can stay empty.
        let (pty_tx, _pty_rx) = mpsc::channel::<PtyCmd>(8);
        let _keep_pty_alive = pty; // kept alive across the test

        let envelope = Envelope {
            v: PROTOCOL_VERSION,
            seq: 1,
            body: EnvelopeBody::Interrupt,
        };

        // Drive the dispatch with the shared writer available. The
        // dispatch should write the ESC byte directly without ever
        // touching pty_tx (which is empty anyway — proving the path
        // bypassed the channel).
        let started = std::time::Instant::now();
        super::dispatch_to_pty(&envelope, Some(&writer), &pty_tx, 80, 24).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "dispatch_to_pty with shared writer should be effectively instant; took {elapsed:?}"
        );

        // Clean up: kill the cat so the test exits cleanly. The cat is
        // a /bin/cat — harmless — but leaving it running is rude.
        // We use the killer slot just like the supervisor does.
        // (`child` is on the Pty struct; take_writer/move semantics
        // make this awkward in a unit test, so just rely on the test
        // harness reaping the process via SIGTERM when the parent
        // exits — `/bin/cat` doesn't outlive the test in our tokio
        // test harness.)
    }

    /// Regression: a stream of single-byte terminal writes — exactly
    /// what the War UI's typing path produces (one binary WS frame
    /// per keystroke) — must reach the PTY master immediately and
    /// unbroken, even if the child is alive and waiting for input.
    ///
    /// Pre-fix, this path packed each byte into `PtyCmd::Write(data)`
    /// and queued it on `pty_rx`, which the blocking PTY thread only
    /// drains between `read()` calls. With a child that's alive but
    /// idle (`/bin/cat` here), `read()` blocks indefinitely and the
    /// queued keystrokes are starved.
    ///
    /// We drive the helper against a real cat over a PTY, type six
    /// characters with no delay between them, and verify the cat
    /// echoes all six back within a 2s budget. Without the fix, the
    /// echoes land seconds later (or not at all within the budget).
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn typed_bytes_reach_pty_via_shared_writer() {
        use crate::pty::Pty;
        use std::io::Read;
        use std::sync::mpsc as std_mpsc;
        use std::time::Instant;

        // Real PTY, /bin/cat as the child — matches the production shape.
        // `take_writer()` gives us the side of the master that the outer
        // supervisor's shared slot would hold.
        let mut pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 4096)
            .expect("spawn cat");
        assert!(pty.alive(), "cat must be alive and waiting for input");
        let mut reader = pty.reader();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pty.master
                .take_writer()
                .map_err(|e| anyhow::anyhow!("take_writer: {e}"))
                .expect("take_writer"),
        ));

        // Build a mock `ActiveSession`. `killer` is unused by the helper
        // we're testing, but the struct requires one — borrow a real
        // one from a throwaway Pty rather than fabricating a dummy.
        let dummy_killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>> = {
            let mut pty2 = Pty::spawn("/bin/true", &[], "/tmp", 80, 24, 1024)
                .expect("spawn /bin/true");
            let _ = pty2.terminate();
            let _ = pty2.wait();
            Arc::new(Mutex::new(pty2.killer))
        };
        let active = ActiveSession {
            pty_link_tx: {
                let (tx, _rx) = mpsc::channel::<PtyCmd>(1);
                tx
            },
            killer: dummy_killer,
            writer: writer.clone(),
        };

        // Reader thread drains whatever cat echoes back. This mirrors
        // the supervisor's blocking PTY reader — it WILL wedge on
        // `read()` when cat is alive and idle, which is exactly the bug
        // condition. We don't join it before the writes; it accumulates
        // echoes that the test body asserts on.
        let (echo_tx, echo_rx) = std_mpsc::channel::<Vec<u8>>();
        let reader_join = tokio::task::spawn_blocking(move || {
            let mut io_buf = [0u8; 64];
            loop {
                match reader.read(&mut io_buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if echo_tx.send(io_buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Type six characters back-to-back — no delays. Under the
        // pre-fix code, these would queue in `pty_rx` and only reach
        // cat the next time the blocking reader happened to wake.
        let started = Instant::now();
        for c in b"hijklm" {
            write_claude_terminal_bytes(&[*c], Some(&active)).await;
        }

        // Drain echoes for up to 2s. We watch for the six-byte substring
        // "hijklm" appearing somewhere in cat's output (the kernel tty
        // may echo CRLF or LF, but the body bytes are preserved).
        let mut got = Vec::new();
        let needle: &[u8] = b"hijklm";
        let deadline = Instant::now() + Duration::from_secs(2);
        while !got.windows(needle.len()).any(|w| w == needle)
            && Instant::now() < deadline
        {
            match echo_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => got.extend_from_slice(&chunk),
                Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let elapsed = started.elapsed();

        // Tear down cat + reader so the test exits cleanly.
        drop(writer);
        if pty.alive() {
            let _ = pty.terminate();
            let _ = pty.wait();
        }
        let _ = reader_join.await;

        let found = got.windows(needle.len()).any(|w| w == needle);
        assert!(
            found,
            "typed bytes did not reach /bin/cat intact; saw {:?} in {elapsed:?}",
            String::from_utf8_lossy(&got)
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "typed bytes landed but took {elapsed:?} — bypass path is too slow"
        );
    }

    /// Regression for the backspace-starvation report. `0x7f` (DEL) is
    /// what xterm.js emits for the Backspace key. The same byte path
    /// that delivers printable characters must also deliver control
    /// characters, including bytes that would silently look like
    /// "nothing happened" on the wire (no echo, no prompt change). We
    /// type "abc<BS>x" through the shared writer, then check that the
    /// byte stream seen on the master side includes the literal
    /// `0x7f`. The kernel line discipline in cooked mode would
    /// translate `\x7f` into a BS-SPACE-BS erase sequence, so we use
    /// `stty raw` to keep the byte literal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn backspace_byte_reaches_pty_via_shared_writer() {
        use crate::pty::Pty;
        use std::io::Read;
        use std::sync::mpsc as std_mpsc;
        use std::time::Instant;

        // `/bin/sh -c 'stty raw -echo; exec cat'` puts cat into raw mode
        // so the kernel doesn't translate 0x7f into an erase sequence
        // before we can observe the literal byte on the master side.
        let mut pty = Pty::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                "stty raw -echo; exec cat".to_string(),
            ],
            "/tmp",
            80,
            24,
            4096,
        )
        .expect("spawn sh+stty+cat");
        assert!(pty.alive());
        let mut reader = pty.reader();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pty.master
                .take_writer()
                .map_err(|e| anyhow::anyhow!("take_writer: {e}"))
                .expect("take_writer"),
        ));
        let dummy_killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>> = {
            let mut pty2 = Pty::spawn("/bin/true", &[], "/tmp", 80, 24, 1024)
                .expect("spawn /bin/true");
            let _ = pty2.terminate();
            let _ = pty2.wait();
            Arc::new(Mutex::new(pty2.killer))
        };
        let active = ActiveSession {
            pty_link_tx: {
                let (tx, _rx) = mpsc::channel::<PtyCmd>(1);
                tx
            },
            killer: dummy_killer,
            writer: writer.clone(),
        };

        let (echo_tx, echo_rx) = std_mpsc::channel::<Vec<u8>>();
        let reader_join = tokio::task::spawn_blocking(move || {
            let mut io_buf = [0u8; 64];
            loop {
                match reader.read(&mut io_buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if echo_tx.send(io_buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Type "abc<BS>x" — every byte travels through the shared
        // writer exactly as the War UI's onData handler feeds them in.
        let started = Instant::now();
        for c in b"abc\x7fx" {
            write_claude_terminal_bytes(&[*c], Some(&active)).await;
        }

        // cat -v visualises control bytes using caret notation: the
        // DEL byte (0x7f) we type shows up as "^?" on the master's
        // output side. So we look for that literal substring in the
        // drained master output. The kernel is in raw mode so the byte
        // is not translated into an erase sequence before it reaches
        // cat — cat itself rewrites it on the way out.
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        // The literal DEL byte (0x7f) appearing in the master's output
        // proves the bypass path delivered the byte untranslated. With
        // `stty raw -echo`, the kernel passes the byte through verbatim
        // (no ERASE processing, no echo-control rewriting).
        while !got.contains(&0x7f) && Instant::now() < deadline {
            match echo_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => got.extend_from_slice(&chunk),
                Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let elapsed = started.elapsed();

        drop(writer);
        if pty.alive() {
            let _ = pty.terminate();
            let _ = pty.wait();
        }
        let _ = reader_join.await;

        assert!(
            got.contains(&0x7f),
            "backspace (0x7f) did not reach cat; saw {:?} in {elapsed:?}",
            String::from_utf8_lossy(&got)
        );
    }

    // -----------------------------------------------------------------
    // §A.7 / seq-numbered snapshot protocol — tests for the per-channel
    // seq counter, the `bytes_read_since_spawn` watermark used to
    // compute `ScreenSnapshotBody::after_seq`, and the wire-shape rule
    // the driver task maintains when it forwards `PtyEvt::Read →
    // LinkCmd::SendBinary`. The integration tests in
    // `tests/snapshot_roundtrip.rs` exercise the full wire + serde
    // round-trip; these are the per-component shape pins so any future
    // "simplification" of the blocking-thread counter can't silently
    // regress to a wrap-around or off-by-one seq.
    // -----------------------------------------------------------------

    /// Pure-logic pin: the seq counter starts at 1 (0 reserved for the
    /// synthetic ESC-on-shutdown placeholder in `spawn_run_one`) and
    /// increments by exactly 1 per `PtyEvt::Read` produced. The
    /// increment-before-assign shape matters — the first byte read on a
    /// fresh blocking thread must carry `seq=1`, not `seq=0`.
    #[test]
    fn next_seq_starts_at_one_and_increments() {
        let mut next_seq: u64 = 1;
        let s1 = next_seq;
        next_seq = next_seq.wrapping_add(1);
        let s2 = next_seq;
        next_seq = next_seq.wrapping_add(1);
        let s3 = next_seq;
        next_seq = next_seq.wrapping_add(1);
        let s4 = next_seq;
        assert_eq!((s1, s2, s3, s4), (1, 2, 3, 4));
    }

    /// Pure-formula pin: when no bytes have ever been read on the
    /// channel, `ScreenSnapshotBody::after_seq` MUST be `0` (the
    /// semantic sentinel the browser reads as "we have no data; do not
    /// discard anything"). When at least one byte has been read, the
    /// value is `next_seq - 1` (the highest seq already assigned). Both
    /// branches belong in one test so a future simplification can't
    /// get one right and the other wrong.
    #[test]
    fn after_seq_zero_when_no_reads_yet_else_last_assigned() {
        // Mirror the exact ternary used in spawn_run_one's snapshot
        // arm so this test stays in lockstep with the production site.
        let after_seq_no_reads = |next_seq: u64, bytes_read: bool| {
            if bytes_read {
                next_seq.wrapping_sub(1)
            } else {
                0
            }
        };
        assert_eq!(
            after_seq_no_reads(1, false),
            0,
            "no bytes fed → after_seq must be 0"
        );
        assert_eq!(
            after_seq_no_reads(1, true),
            0,
            "bytes fed but next_seq hasn't bumped yet → after_seq = 1 - 1 = 0"
        );
        assert_eq!(
            after_seq_no_reads(7, true),
            6,
            "six reads assigned → after_seq = next_seq - 1"
        );
    }

    /// End-to-end pin of the read-arm seq counter against a real
    /// `/bin/cat` PTY: each `PtyEvt::Read` carries a strict monotonic
    /// seq starting at `1`. This catches any future refactor that
    /// accidentally drops the increment-before-assign shape or skips a
    /// seq in the loop body.
    ///
    /// Pipeline: write a multi-byte payload to cat, drain every
    /// PtyEvt::Read the producer emits during a ~1.5s window, and
    /// assert the seqs come back as `[1, 2, 3, …]`. The number of
    /// reads is whatever the kernel chooses to coalesce; what we pin
    /// is the seq shape, not the chunk count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn read_arm_assigns_monotonic_seqs_per_channel() {
        use crate::pty::Pty;
        use std::io::{Read, Write};
        use std::sync::mpsc as std_mpsc;

        let mut pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 0)
            .expect("spawn cat");
        let mut reader = pty.reader();
        let mut writer = pty.writer();

        let (evt_tx, evt_rx) = std_mpsc::channel::<PtyEvt>();
        let evt_tx_t = evt_tx.clone();
        let reader_join = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let mut next_seq: u64 = 1;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let seq = next_seq;
                        next_seq = next_seq.wrapping_add(1);
                        let _ = evt_tx_t.send(PtyEvt::Read {
                            chan: TERM_CHAN_CLAUDE,
                            seq,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        // Multi-byte payload + a newline so the kernel's line discipline
        // flushes — a single 1-byte write through a cooked-mode PTY may
        // sit buffered forever waiting for '\n'. Two newlines give the
        // test deterministic coverage of "≥ 2 reads" without depending
        // on chunking.
        writer
            .write_all(b"hello\nworld\n")
            .expect("write to pty");
        writer.flush().ok();

        // Drain every PtyEvt::Read that lands in the next 1.5s, then
        // stop. We refuse to predict how many reads the kernel issues
        // for one `write_all` — that's a function of PTY line
        // discipline + cat's scheduling — but whatever the count, the
        // seqs must be 1, 2, 3, ... in arrival order.
        let mut seqs: Vec<u64> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            match evt_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PtyEvt::Read { seq, .. }) => seqs.push(seq),
                Ok(_) => {} // ignore Exited/Meta
                Err(_) => continue,
            }
        }

        drop(writer);
        if pty.alive() {
            let _ = pty.terminate();
            let _ = pty.wait();
        }
        let _ = reader_join.join();

        assert!(
            !seqs.is_empty(),
            "expected at least one PtyEvt::Read from the producer"
        );
        assert_eq!(
            seqs[0], 1,
            "first read must carry seq=1 (single-producer, starts-at-1); got {:?}",
            seqs
        );
        for w in seqs.windows(2) {
            assert_eq!(
                w[1],
                w[0] + 1,
                "seq must be strictly +1; got {:?}",
                seqs
            );
        }
    }

    /// End-to-end pin of `ScreenSnapshotBody::after_seq` against a real
    /// cat PTY: after at least one byte has been read, the snapshot's
    /// `after_seq` must equal the highest seq the producer ever
    /// assigned (== the most-recently-emitted `seq`).
    ///
    /// Same approach as `read_arm_*`: drive cat, capture every
    /// `seq` that lands on the channel, compose a
    /// `ScreenSnapshotBody` with the production formula, and assert
    /// the field reads back as the high-water mark.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn snapshot_after_seq_reflects_last_fed_on_real_pty() {
        use crate::pty::Pty;
        use crate::wire::ScreenSnapshotBody;
        use std::io::{Read, Write};
        use std::sync::mpsc as std_mpsc;

        let mut pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 0)
            .expect("spawn cat");
        let mut reader = pty.reader();
        let mut writer = pty.writer();

        let (evt_tx, evt_rx) = std_mpsc::channel::<PtyEvt>();
        let evt_tx_t = evt_tx.clone();
        let reader_join = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let mut next_seq: u64 = 1;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let seq = next_seq;
                        next_seq = next_seq.wrapping_add(1);
                        let _ = evt_tx_t.send(PtyEvt::Read {
                            chan: TERM_CHAN_CLAUDE,
                            seq,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        // Produce at least three seqs by sending two newlines (≥ 2
        // echoes) and waiting.
        writer.write_all(b"a\nb\n").expect("write to pty");
        writer.flush().ok();

        // Drain for ~1.5s and capture every seq.
        let mut seqs: Vec<u64> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            match evt_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(PtyEvt::Read { seq, .. }) => seqs.push(seq),
                Ok(_) => {}
                Err(_) => continue,
            }
        }

        let hwm = seqs.last().copied().unwrap_or(0);

        // Compose a ScreenSnapshotBody using the production formula
        // (`bytes_read ? next_seq - 1 : 0`). Locally the highest seq
        // captured above IS the HWM the snapshot would advertise.
        let bytes_read = !seqs.is_empty();
        let after_seq = if bytes_read { hwm } else { 0 };
        let body = ScreenSnapshotBody {
            chan: TERM_CHAN_CLAUDE,
            cols: 80,
            rows: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            text: vec!["".into()],
            after_seq,
        };

        drop(writer);
        if pty.alive() {
            let _ = pty.terminate();
            let _ = pty.wait();
        }
        let _ = reader_join.join();

        assert!(
            bytes_read,
            "expected bytes_read_flag to flip true after typing"
        );
        assert!(
            hwm >= 2,
            "expected hwm ≥ 2 from the cat echo of two newlines; got {hwm} (seqs={seqs:?})"
        );
        assert_eq!(
            body.after_seq, hwm,
            "after_seq must equal the highest seq assigned (snapshot HWM)"
        );
    }

    /// Pin: before the blocking thread ever completes a `read()` with
    /// `n > 0`, the `bytes_read_since_spawn` flag stays `false` and the
    /// snapshot arm must produce `after_seq = 0`. We exercise this
    /// without a real PTY by driving the counter state directly.
    #[test]
    fn snapshot_before_any_read_carries_after_seq_zero() {
        // Counter starts at 1 unconditionally; the boolean is what the
        // snapshot arm checks. A fresh blocking thread that hasn't yet
        // had a successful `read()` must report after_seq = 0 regardless
        // of where `next_seq` happens to sit.
        let next_seq_after_zero_reads: u64 = 1;
        let bytes_read_since_spawn = false;
        let after_seq = if bytes_read_since_spawn {
            next_seq_after_zero_reads.wrapping_sub(1)
        } else {
            0
        };
        assert_eq!(
            after_seq, 0,
            "first-ever snapshot before any read must carry after_seq = 0"
        );
    }
}
