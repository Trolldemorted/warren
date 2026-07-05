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
use crate::recorder::AsciicastRecorder;
use crate::respawn::{self, CrashWindow};
use crate::shell::{self, ShellCmd, ShellHandle};
use crate::wire::{
    Envelope, EnvelopeBody, LogLine, ScreenSnapshotBody, StateFrame, TermSize, TERM_CHAN_CLAUDE,
    TERM_CHAN_SHELL,
};
use anyhow::Result;
use parking_lot::Mutex;
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

    // §D Milestone 5: tiny HTTP server for fetching `.cast` recordings.
    // Binds 0.0.0.0 so warren can reach it across pods/hosts; the bearer
    // token gates all non-healthz endpoints. Off when the recorder is off.
    if config.enable_asciicast {
        let port = config.recorder_http_port;
        let dir = config.asciicast_dir.clone();
        let token = config.agent_token.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::http_server::serve(port, dir, token).await {
                log::error!("recorder http stopped: {e:?}");
            }
        });
    }

    let hook_bin = hooks_install::resolve_hook_bin(config.hook_bin.clone());
    if let Err(e) = hooks_install::install(std::path::Path::new(&config.workdir), &hook_bin) {
        log::warn!("could not install hook settings.json: {e:?}");
    }

    let agent_id = Uuid::new_v4();
    let claude_version = detect_claude_version(&config).await;

    let replay_buf: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let snap_buf = replay_buf.clone();
    let replay_snap: ReplaySnapFn = Arc::new(move || {
        let buf = snap_buf.lock();
        let mut out = Vec::new();
        for chunk in buf.iter() {
            out.extend_from_slice(chunk);
        }
        out
    });

    let (cmd_tx, cmd_rx) = mpsc::channel::<LinkCmd>(128);
    let (event_tx, mut event_rx) = mpsc::channel::<LinkEvent>(128);

    let meta_ring = Arc::new(MetaRing::new(config.meta_ring_bytes));

    // §D Milestone 5: advertise the recorder HTTP base URL to warren via
    // the Hello envelope so `/agent/:id/claude/history` can fetch `.cast`
    // files without a static per-agent config. `RABBIT_RECORDER_URL`
    // overrides the auto-derived `http://0.0.0.0:{port}` (useful when the
    // recorder binds 0.0.0.0 but is reachable via a service IP / DNS name).
    let recorder_url = if config.enable_asciicast {
        std::env::var("RABBIT_RECORDER_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                format!("http://0.0.0.0:{}", config.recorder_http_port)
            })
    } else {
        String::new()
    };
    let recorder_url_opt = if recorder_url.is_empty() {
        None
    } else {
        Some(recorder_url)
    };

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
        recorder_url_opt,
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
            // §D Milestone 5: asciicast recorder sidecar (opt-in via
            // `enable_asciicast`). Lives one claude generation — `start_session`
            // is fired by the `SessionStart` hook event, `feed` by every
            // PTY read chunk, `close` on `PtyEvt::Exited`. None when disabled
            // so production pods pay nothing for it.
            let recorder = if config.enable_asciicast {
                if let Err(e) = std::fs::create_dir_all(&config.asciicast_dir) {
                    log::warn!(
                        "asciicast_dir create failed ({}): {e:?}; recorder disabled",
                        config.asciicast_dir.display()
                    );
                    None
                } else {
                    Some(AsciicastRecorder::new(
                        config.asciicast_dir.clone(),
                        config.asciicast_bytes_cap,
                        config.term_cols,
                        config.term_rows,
                    ))
                }
            } else {
                None
            };
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
                recorder,
            ) {
                Ok(sess) => {
                    let OutcomeChannels {
                        pty_link_tx,
                        outcome_rx_in,
                    } = sess.outcome_channels;
                    outcome_rx = outcome_rx_in;
                    active = Some(ActiveSession { pty_link_tx });
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
                            if let Some(tx) = &active_link_tx {
                                let _ = tx.send(PtyCmd::Terminate).await;
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
                                dispatch_to_pty(&env, tx, config.term_cols, config.term_rows).await;
                            }
                        }
                    }
                    Some(LinkEvent::Binary { chan, data }) => {
                        if chan == TERM_CHAN_SHELL {
                            if let Some(sh) = &shell {
                                let _ = sh.tx.send(ShellCmd::Write(data)).await;
                            }
                        } else if let Some(tx) = &active_link_tx {
                            let _ = tx.send(PtyCmd::Write(data)).await;
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
    Read(Vec<u8>),
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
}

struct OutcomeChannels {
    pty_link_tx: mpsc::Sender<PtyCmd>,
    outcome_rx_in: mpsc::Receiver<RunOutcome>,
}

struct SpawnResult {
    outcome_channels: OutcomeChannels,
}

#[allow(clippy::too_many_arguments)]
fn spawn_run_one(
    config: &Config,
    health: HealthState,
    agent_id: Uuid,
    claude_version: &str,
    observer: ObserverHandle,
    args: Vec<String>,
    replay_buf: Arc<Mutex<VecDeque<Vec<u8>>>>,
    cmd_tx: mpsc::Sender<LinkCmd>,
    shutdown: Arc<AtomicBool>,
    recorder: Option<AsciicastRecorder>,
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

    let pty_join = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut pty = Pty::spawn(&bin, &args, &workdir, cols, rows, replay)?;
        let mut reader = pty.reader();
        let mut writer = pty.writer();
        let initial_replay = pty.snapshot_replay().to_vec();

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
        loop {
            if shutdown_for_blocking.load(Ordering::SeqCst) {
                graceful_pending = true;
                if graceful_since.is_none() {
                    graceful_since = Some(Instant::now());
                    log::info!("shutdown: sending ESC + waiting up to {grace_period:?}");
                    let _ = writer.write_all(b"\x1b");
                    let _ = writer.flush();
                    let _ = pty_evt_tx.blocking_send(PtyEvt::Read(b"ESC (shutdown)".to_vec()));
                }
            }

            if let Ok(cmd) = pty_rx.try_recv() {
                match cmd {
                    PtyCmd::Write(b) => {
                        use std::io::Write;
                        if writer.write_all(&b).is_err() {
                            break;
                        }
                        let _ = writer.flush();
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
                        let body = ScreenSnapshotBody {
                            chan,
                            cols: snap.cols,
                            rows: snap.rows,
                            cursor_col: snap.cursor_col,
                            cursor_row: snap.cursor_row,
                            cursor_visible: snap.cursor_visible,
                            text: snap.text,
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
                            let _ = writer.write_all(b"\x1b");
                            let _ = writer.flush();
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
                Ok(0) => break,
                Ok(n) => {
                    vt.feed(&io_buf[..n]);
                    if let Some(tw) = trust_watcher.as_mut() {
                        if let Some(resp) = tw.observe(&io_buf[..n]) {
                            log::info!("trust dialog detected; auto-accepting with Enter");
                            let _ = writer.write_all(resp);
                            let _ = writer.flush();
                        }
                    }
                    if pty_evt_tx
                        .blocking_send(PtyEvt::Read(io_buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
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
    // §D Milestone 5: recorder sidecar moved into the driver task. Lives
    // exactly one claude generation: started by SessionStart hook event,
    // fed by every PTY read, closed on Exited. None when disabled.
    let mut recorder = recorder;

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
                        Some(PtyEvt::Read(c)) => {
                            {
                                let mut buf = replay_buf_inner.lock();
                                buf.push_back(c.clone());
                                trim_replay(&mut buf, replay_cap_inner);
                            }
                            // §D Milestone 5: mirror the same byte stream the
                            // replay buffer just recorded into the asciicast
                            // sidecar. One source of truth — the recorder sees
                            // exactly what warren's browser sees.
                            if let Some(r) = recorder.as_mut() {
                                r.feed(&c).await;
                            }
                            let _ = cmd_tx_driver
                                .send(LinkCmd::SendBinary {
                                    chan: TERM_CHAN_CLAUDE,
                                    data: c,
                                })
                                .await;
                        }
                        Some(PtyEvt::Meta(body)) => {
                            let _ = cmd_tx_driver.send(LinkCmd::SendMeta(body)).await;
                        }
                        Some(PtyEvt::Exited(status)) => {
                            log::info!("claude exited: kind={:?}", ExitKind::from(&status));
                            // Flush + close the recorder before reporting the
                            // outcome — guarantees the .cast is on disk by the
                            // time the supervisor's outer loop considers this
                            // generation done.
                            if let Some(r) = recorder.as_mut() {
                                r.close().await;
                            }
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
                        // §D Milestone 5: open a fresh .cast when SessionStart
                        // fires. The observer emits `kind == "session"` with a
                        // non-None `session_id` from the SessionStart hook
                        // payload (see `observer::hooks::parse`).
                        if ev.kind == "session" {
                            if let (Some(r), Some(sid)) = (recorder.as_mut(), ev.session_id.as_deref()) {
                                if let Err(e) = r.start_session(sid).await {
                                    log::warn!("asciicast start_session({sid}) failed: {e:?}");
                                }
                            }
                        }
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

async fn dispatch_to_pty(env: &Envelope, pty_tx: &mpsc::Sender<PtyCmd>, cols: u16, rows: u16) {
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
            EnvelopeBody::Clear { hard: _ } => {
                let _ = input::slash(&mut shim, "clear");
            }
            EnvelopeBody::Resize { cols: rc, rows: rr } => {
                let _ = pty_tx.try_send(PtyCmd::Resize {
                    cols: *rc,
                    rows: *rr,
                });
                return;
            }
            EnvelopeBody::Repaint => {
                let _ = pty_tx.try_send(PtyCmd::Repaint { cols, rows });
                return;
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
    if let Some(st) = State::from_label(&frame.state) {
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
fn trim_replay(buf: &mut VecDeque<Vec<u8>>, cap: usize) {
    let mut total: usize = buf.iter().map(|v| v.len()).sum();
    while total > cap {
        match buf.pop_front() {
            Some(front) => total -= front.len(),
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
        let mut buf: VecDeque<Vec<u8>> = VecDeque::new();
        for _ in 0..10 {
            buf.push_back(vec![0u8; 50]);
        }
        trim_replay(&mut buf, 200);
        let total: usize = buf.iter().map(|v| v.len()).sum();
        assert!(total <= 200);
    }

    #[test]
    fn trim_replay_no_op_when_under_cap() {
        let mut buf: VecDeque<Vec<u8>> = VecDeque::new();
        buf.push_back(b"hello".to_vec());
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
                state: "idle".into(),
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
                state: "dead".into(),
                session_id: None,
                reason: Some("crashed".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(observer.latest_state(), State::Dead);

        // An unrecognized label must leave the tracked state untouched.
        send_state(
            &observer,
            &tx,
            StateFrame {
                state: "gibberish".into(),
                session_id: None,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(observer.latest_state(), State::Dead);
    }
}
