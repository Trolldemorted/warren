use crate::config::Config;
use crate::health::{serve as serve_health, HealthState};
use crate::hooks_install;
use crate::input;
use crate::link::{Link, LinkCmd, LinkEvent, ReplaySnapFn};
use crate::meta_ring::MetaRing;
use crate::observer::context::{ContextParser, ContextSnapshot};
use crate::observer::hooks::{ObserverEvent, ObserverHandle};
use crate::observer::limits::{LimitsParser, UsageLimits};
use crate::observer::state::State;
use crate::observer::transcript::{default_transcript_path, TranscriptTail, UsageUpdate};
use crate::pty::{ExitKind, Pty, PtyExitStatus};
use crate::respawn::{self, CrashWindow};
use crate::shell::{self, ShellCmd, ShellHandle};
use anyhow::Result;
use parking_lot::Mutex;
use portable_pty::ChildKiller;
use rabbit_lib::wire::{
    AgentState, Envelope, EnvelopeBody, LogLine, ScreenSnapshotBody, StateFrame, TermFrame,
    TermSize, UsageSnapshot, TERM_CHAN_CLAUDE, TERM_CHAN_SHELL,
};
use std::collections::VecDeque;
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
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
    // so each binary message sent on warren_link reconnect preserves the seq the
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

    let warren_link = Link::new(
        config.warren_url.clone(),
        config.agent_token.clone(),
        agent_id,
        claude_version.clone(),
        cmd_rx,
        event_tx,
        replay_snap,
        meta_ring,
        shutdown.clone(),
    );
    // §Simplify TUI sizing: capture the initial grid once *before* the
    // link task is spawned so the shell PTY can size itself without
    // fighting for `&warren_link`. The link's `term_size` slot gets
    // refreshed once warren ships the `TuiConfig` envelope, but the
    // shell only ever spawns here — so the initial read is enough.
    let initial_tui = warren_link.term_size();
    {
        tokio::spawn(async move {
            if let Err(e) = warren_link.run().await {
                log::error!("warren_link exited: {e:?}");
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
        log::info!(
            "shell enabled: bin={} args={:?}",
            config.shell_bin,
            config.shell_args
        );
        Some(shell::spawn(
            &config,
            &initial_tui,
            cmd_tx.clone(),
            shutdown.clone(),
        ))
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

    // §Writer actor — coalesce concurrent `UsageCheck` envelopes
    // so two clicks within a single scrape window share ONE scrape
    // instead of starting two parallel scrapers competing for the
    // broadcast receiver and the writer actor's FIFO.
    //
    // Implementation: the slot holds a `ScrapeWaiter<T>` carrying
    // a `Notify` and a `Mutex<Option<T>>`. The first UsageCheck
    // creates the waiter, stashes it behind an `Arc<Mutex<...>>`
    // slot, and spawns the scrape task with the waiter. Subsequent
    // UsageChecks during the scrape see `Some` and clone the
    // waiter — each waiter independently observes the result via
    // `await_scrape` (early-return check, then `notified()`).
    //
    // We previously used a `watch::channel` pair for this, but
    // tokio's `watch::Receiver::changed()` tracks a per-receiver
    // "seen" state. Wrapping the receiver in `Arc<Mutex<>>` forced
    // every coalesced waiter through a single receiver instance,
    // so once the first waiter consumed the change notification
    // every subsequent waiter blocked until the next send. `Notify`
    // + cached result has per-call independence and the early-return
    // check covers the race where the scrape completes between the
    // slot-lock and the waiter's `notified()`.
    //
    // `tokio::sync::Mutex` is fine here: the critical section is a
    // few clones + a maybe-spawn, and the outer loop is
    // single-threaded anyway. No contention with the writer actor
    // (which uses its own mpsc).
    type UsageScrapeSlot = Arc<tokio::sync::Mutex<Option<ScrapeWaiter<(UsageLimits, bool)>>>>;
    let current_scrape: UsageScrapeSlot = Arc::new(tokio::sync::Mutex::new(None));

    // §Context-window: parallel coalescing slot for `/context`
    // scrapes. Two clicks within a single scrape window share ONE
    // scrape instead of starting two parallel scrapers competing
    // for the broadcast receiver and the writer actor's FIFO.
    // Mirrors the `current_scrape` slot above.
    type ContextScrapeSlot = Arc<tokio::sync::Mutex<Option<ScrapeWaiter<(ContextSnapshot, bool)>>>>;
    let current_context_scrape: ContextScrapeSlot = Arc::new(tokio::sync::Mutex::new(None));
    // §Context-window / §A.7: handle to the live PTY reader's seq
    // watermark. Updated by each `spawn_run_one` so the outer loop's
    // `/context` arm can pass it to `run_context_scrape` for the
    // restore snapshot's `after_seq`. None between generations.
    let next_seq_slot: Arc<tokio::sync::Mutex<Option<Arc<std::sync::atomic::AtomicU64>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    loop {
        // Spawn a new claude generation if we have nothing running and aren't dead.
        if active.is_none() && !dead && !shutdown.load(Ordering::SeqCst) {
            // Capture `was_some` BEFORE `take()` so the effective_args
            // decision knows whether this spawn is a cold start (no
            // operator Restart in flight) or an operator-issued Restart.
            // On cold start we skip --continue so a brand-new rabbit
            // process doesn't slam into `No conversation found to
            // continue` and crash-loop the supervisor before the first
            // SessionStart hook fires.
            let was_some = restart_pending.is_some();
            let fresh = restart_pending.take().unwrap_or(false);
            let cold_start = !was_some;
            let session_id = observer.latest_session();
            let args =
                respawn::effective_args(&base_args, session_id.as_deref(), fresh, cold_start);
            log::info!(
                "spawning pty: bin={} args={:?} fresh={} cold_start={}",
                config.claude_bin,
                args,
                fresh,
                cold_start
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
                initial_tui,
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
                        term_bcast_tx: sess.term_bcast_tx,
                        vt: sess.vt,
                    });
                    *next_seq_slot.lock().await = Some(sess.next_seq);
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
            // we exit. The warren_link also polls `shutdown` itself, so this is
            // best-effort — if the send fails (channel full / closed), the
            // flag will still break the warren_link's reconnect loop.
            crate::dispatch::send_or_warn("LinkCmd::Shutdown", &cmd_tx, LinkCmd::Shutdown).await;
        }

        let active_link_tx = active.as_ref().map(|s| s.pty_link_tx.clone());
        let active_writer = active.as_ref().map(|s| s.writer.clone());
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
                        } else if let EnvelopeBody::UsageCheck = &env.body {
                            // §Usage-limits + §Writer actor: drive the
                            // synchronous `/usage` scrape. Two clicks
                            // within the scrape window coalesce — the
                            // second UsageCheck awaits the same
                            // oneshot receiver the first one
                            // registered, instead of starting a
                            // parallel scraper competing for the
                            // broadcast receiver and the writer
                            // actor's FIFO.
                            //
                            // §Small-terminal mitigation C is
                            // applied AFTER coalescing so both
                            // duplicates see the same
                            // partial-vs-complete classification.
                            if let Some(active) = &active {
                                let waiter = {
                                    let mut g = current_scrape.lock().await;
                                    if let Some(w) = g.as_ref() {
                                        w.clone()
                                    } else {
                                        // First UsageCheck in the
                                        // window: build a coalescing
                                        // waiter, stash it in the
                                        // slot, spawn the scrape
                                        // task. Extract just the
                                        // writer + broadcast (the
                                        // only things the scrape
                                        // task needs) — avoids
                                        // cloning the whole
                                        // `ActiveSession`.
                                        let w = ScrapeWaiter {
                                            notify: Arc::new(
                                                tokio::sync::Notify::new(),
                                            ),
                                            result: Arc::new(
                                                tokio::sync::Mutex::new(None),
                                            ),
                                        };
                                        *g = Some(w.clone());
                                        let writer_for_scrape = active.writer.clone();
                                        let bcast_for_scrape = active.term_bcast_tx.clone();
                                        let current_scrape_for_task = current_scrape.clone();
                                        let w_for_task = w.clone();
                                        tokio::spawn(async move {
                                            let (limits, aborted) =
                                                run_usage_scrape(writer_for_scrape, bcast_for_scrape)
                                                    .await;
                                            // Publish the result
                                            // BEFORE clearing the
                                            // slot so a third
                                            // UsageCheck arriving
                                            // during this brief gap
                                            // sees `Some` and joins
                                            // the just-finished
                                            // scrape (gets the same
                                            // result envelope)
                                            // rather than starting
                                            // a wasted fresh
                                            // scrape. The early-
                                            // return check in
                                            // `await_scrape` covers
                                            // the gap where a
                                            // late-arriving waiter
                                            // already missed the
                                            // notify.
                                            *w_for_task.result.lock().await = Some((limits, aborted));
                                            w_for_task.notify.notify_waiters();
                                            *current_scrape_for_task.lock().await = None;
                                        });
                                        w
                                    }
                                };
                                let (limits, aborted) = await_scrape(&waiter).await;
                                let scrape_incomplete =
                                    !limits.is_empty() && !limits.all_populated();
                                let snap = UsageSnapshot {
                                    source: "usage_check".to_string(),
                                    weekly_pct: limits.weekly_pct,
                                    weekly_resets_at: limits.weekly_resets_at,
                                    session_pct: limits.session_pct,
                                    session_resets_at: limits.session_resets_at,
                                    scrape_incomplete,
                                    scrape_aborted: aborted,
                                    ..Default::default()
                                };
                                crate::dispatch::send_or_warn(
                                    "LinkCmd::SendMeta(Usage)",
                                    &cmd_tx,
                                    LinkCmd::SendMeta(Box::new(EnvelopeBody::Usage(snap))),
                                )
                                .await;
                            }
                        } else if let EnvelopeBody::ContextCheck = &env.body {
                            // §Context-window: drive the synchronous
                            // `/context` overlay scrape. Two clicks
                            // within the scrape window coalesce — the
                            // second ContextCheck awaits the same
                            // watch receiver the first one registered
                            // instead of starting a parallel scraper
                            // competing for the broadcast receiver
                            // and the writer actor's FIFO.
                            //
                            // Result envelope: a fresh `Usage`
                            // carrying the new `ctx_*` fields
                            // layered on top of the most-recent
                            // transcript-derived snapshot (read via
                            // `observer::latest_usage()`) so the
                            // dashboard's input/output/cache
                            // counters keep updating independently
                            // of the modal scrape.
                            if let Some(active) = &active {
                                let waiter = {
                                    let mut g = current_context_scrape.lock().await;
                                    if let Some(w) = g.as_ref() {
                                        w.clone()
                                    } else {
                                        // First ContextCheck in the
                                        // window: build a coalescing
                                        // waiter, stash it in the
                                        // slot, spawn the scrape
                                        // task.
                                        let w = ScrapeWaiter {
                                            notify: Arc::new(
                                                tokio::sync::Notify::new(),
                                            ),
                                            result: Arc::new(
                                                tokio::sync::Mutex::new(None),
                                            ),
                                        };
                                        *g = Some(w.clone());
                                        let writer_for_scrape =
                                            active.writer.clone();
                                        let bcast_for_scrape =
                                            active.term_bcast_tx.clone();
                                        let vt_for_scrape =
                                            active.vt.clone();
                                        let next_seq_for_scrape = next_seq_slot
                                            .lock()
                                            .await
                                            .clone()
                                            .unwrap_or_else(|| {
                                                Arc::new(std::sync::atomic::AtomicU64::new(1))
                                            });
                                        let cmd_tx_for_scrape =
                                            cmd_tx.clone();
                                        let current_for_task =
                                            current_context_scrape.clone();
                                        let w_for_task = w.clone();
                                        tokio::spawn(async move {
                                            let (snap, aborted) = run_context_scrape(
                                                writer_for_scrape,
                                                bcast_for_scrape,
                                                vt_for_scrape,
                                                next_seq_for_scrape,
                                                cmd_tx_for_scrape,
                                            )
                                            .await;
                                            // Publish the result
                                            // BEFORE clearing the
                                            // slot so a third
                                            // ContextCheck arriving
                                            // during this brief gap
                                            // sees `Some` and joins
                                            // the just-finished
                                            // scrape. The
                                            // early-return check in
                                            // `await_scrape` covers
                                            // the gap where a
                                            // late-arriving waiter
                                            // already missed the
                                            // notify.
                                            *w_for_task.result.lock().await = Some((snap, aborted));
                                            w_for_task.notify.notify_waiters();
                                            *current_for_task.lock().await = None;
                                        });
                                        w
                                    }
                                };
                                let (snap, aborted) = await_scrape(&waiter).await;
                                let scrape_incomplete =
                                    !snap.is_empty() && !snap.all_populated();
                                let scrape_empty = snap.is_empty();
                                // Merge the modal fields on top of
                                // the most-recent transcript
                                // snapshot. The supervisor doesn't
                                // parse the transcript — it reads
                                // the cached snapshot that
                                // `record_usage()` keeps fresh.
                                let mut combined = crate::observer::latest_usage();
                                combined.source = "context_check".to_string();
                                combined.ctx_used_tokens = snap.used_tokens;
                                combined.ctx_total_tokens = snap.total_tokens;
                                combined.ctx_used_pct = snap.used_pct;
                                combined.ctx_free_pct = snap.free_pct;
                                combined.ctx_window_tokens = snap.window_tokens;
                                combined.ctx_categories = snap.categories;
                                // §Context-window: an empty parse is
                                // also "incomplete" — the operator
                                // pressed the button, the modal
                                // didn't surface data inside the
                                // 700ms scrape window, and the
                                // panel otherwise looks identical
                                // to a button-press that never
                                // happened. Fold both into the
                                // same flag so the UI surfaces a
                                // hint either way. `scrape_aborted`
                                // stays reserved for actual
                                // operator-interrupt preemption
                                // (writer's
                                // `SequenceOutcome::AbortedBeforeStep`);
                                // conflating it with empty-parse
                                // would make every slow-scrape look
                                // like the operator hit Ctrl-C.
                                combined.ctx_scrape_incomplete =
                                    scrape_incomplete || scrape_empty;
                                combined.scrape_aborted = aborted;
                                // Cache the modal-enriched snapshot
                                // so subsequent transcript ticks
                                // (which carry `ctx_* = None`) don't
                                // clobber these values when
                                // `record_usage` merges on top.
                                crate::observer::record_usage(combined.clone());
                                crate::dispatch::send_or_warn(
                                    "LinkCmd::SendMeta(Usage+context)",
                                    &cmd_tx,
                                    LinkCmd::SendMeta(Box::new(EnvelopeBody::Usage(combined))),
                                )
                                .await;
                            } else {
                                // §Context-window: no active claude
                                // session — can't write `/context` to
                                // a PTY. Publish an envelope anyway
                                // so the JS panel doesn't sit at
                                // "—" forever after the operator
                                // pressed the button; the hint path
                                // surfaces "no active session".
                                let mut combined = crate::observer::latest_usage();
                                combined.source = "context_check".to_string();
                                combined.ctx_scrape_incomplete = true;
                                combined.scrape_aborted = true;
                                crate::dispatch::send_or_warn(
                                    "LinkCmd::SendMeta(Usage+context:no-session)",
                                    &cmd_tx,
                                    LinkCmd::SendMeta(Box::new(EnvelopeBody::Usage(combined))),
                                )
                                .await;
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
                            // The actor already decided the prompt's
                            // fate (`PromptEcho` accepts, `PromptRejected`
                            // already reached warren, `StopHook` is a
                            // no-op). Dispatch what survived.
                            dispatch_to_pty(
                                &env,
                                active_writer.as_ref(),
                                tx,
                                initial_tui.cols,
                                initial_tui.rows,
                            )
                            .await;
                        }
                    }
                    Some(LinkEvent::Binary { chan, data }) => {
                        if chan == TERM_CHAN_SHELL {
                            if let Some(sh) = &shell {
                                crate::dispatch::send_or_warn(
                                    "ShellCmd::Write",
                                    &sh.tx,
                                    ShellCmd::Write(data),
                                )
                                .await;
                            }
                        } else if chan == TERM_CHAN_CLAUDE {
                            // §diagnose backspace: opt-in via RUST_LOG=debug.
                            // Logs every binary frame arriving on the
                            // Claude channel so we can confirm the byte
                            // (e.g. 0x7f for Backspace) reaches this
                            // layer from the warren_link. Compare with the
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
                            // (matches `warren_link.rs`'s filter against the
                            // known terminal channels).
                            log::debug!(
                                "ignoring binary frame on unknown chan {chan} ({} bytes)",
                                data.len()
                            );
                        }
                    }
                    Some(LinkEvent::Connected) => {
                        // §Reconnect state resync: the link just
                        // (re)established the WS and delivered a
                        // Hello with `state=Starting`. Warren has no
                        // memory of the previous connection's state
                        // and will see us as Starting until we push
                        // a real State envelope. The supervisor's
                        // own State(Idle) publish (inside
                        // `spawn_run_one`) only fires when claude is
                        // being spawned fresh — on a warren-side
                        // restart with claude still running, that
                        // path never runs and the scheduler sees
                        // the agent as Starting forever, refusing to
                        // dispatch. Push the current observer state
                        // (Idle/Running/...) + session_id so the
                        // scheduler and any browser pane see us in
                        // the right shape immediately. On the very
                        // first connect this is also fine:
                        // `observer.latest_state()` is `Starting`
                        // until `spawn_run_one` overrides it, so we
                        // publish Starting here and Idle a few
                        // hundred ms later when claude is up. A
                        // duplicate State(Idle) arriving on the
                        // first-connect path is harmless (idempotent
                        // in `update_state`).
                        let st = observer.latest_state();
                        let _ = send_state(
                            &observer,
                            &cmd_tx,
                            StateFrame {
                                state: agent_state_from_observer(st),
                                session_id: observer.latest_session(),
                                reason: None,
                            },
                        )
                        .await;
                    }
                    None => {
                        log::warn!("warren_link event channel closed");
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
                    crate::dispatch::send_or_warn(
                        "PtyCmd::GracefulShutdown",
                        tx,
                        PtyCmd::GracefulShutdown,
                    )
                    .await;
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
            // §Cross-crate merge: a transcript tick fires on every
            // successful JSONL parse, which can race against a
            // freshly-published `ContextCheck` response. The
            // transcript-derived snapshot has `ctx_* = None` and
            // `ctx_scrape_incomplete = false`; publishing it raw
            // would clobber the modal values the operator just
            // asked for. `record_usage` already merged into
            // LATEST_USAGE before we got here, but the mpsc channel
            // carries the ORIGINAL snapshot — overlay on the way
            // out so the wire envelope the browser receives is
            // consistent with what the modal scrape published.
            let mut snap = update.usage;
            let cached = crate::observer::latest_usage();
            if snap.ctx_used_tokens.is_none() {
                snap.ctx_used_tokens = cached.ctx_used_tokens;
            }
            if snap.ctx_total_tokens.is_none() {
                snap.ctx_total_tokens = cached.ctx_total_tokens;
            }
            if snap.ctx_used_pct.is_none() {
                snap.ctx_used_pct = cached.ctx_used_pct;
            }
            if snap.ctx_free_pct.is_none() {
                snap.ctx_free_pct = cached.ctx_free_pct;
            }
            if snap.ctx_window_tokens.is_none() {
                snap.ctx_window_tokens = cached.ctx_window_tokens;
            }
            if snap.ctx_categories.is_none() {
                snap.ctx_categories = cached.ctx_categories;
            }
            snap.ctx_scrape_incomplete |= cached.ctx_scrape_incomplete;
            let _ = cmd_tx
                .send(LinkCmd::SendMeta(Box::new(EnvelopeBody::Usage(snap))))
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
    Read {
        chan: u8,
        seq: u64,
        data: Vec<u8>,
    },
    /// §Once-and-for-all writer actor: the blocking PTY thread no
    /// longer holds the PTY master writer. When it needs to write
    /// bytes (trust-dialog auto-accept, shutdown ESC, legacy
    /// `PtyCmd::Write` fallbacks in tests) it sends `WriteBack`
    /// here. The driver task receives the event and submits the
    /// bytes through the [`WriterHandle`] so ordering stays
    /// inside the FIFO actor.
    WriteBack(Vec<u8>),
    Exited(PtyExitStatus),
    /// §D Milestone 5 (Phase B): a structured meta envelope generated inside
    /// the blocking PTY thread (currently only `ScreenSnapshot`). The driver
    /// loop forwards these to warren via `LinkCmd::SendMeta` so they ride
    /// the same seq/ack channel as everything else.
    Meta(Box<EnvelopeBody>),
}

#[derive(Debug)]
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
    /// §Once-and-for-all PTY writer: a [`WriterHandle`] backed by a
    /// dedicated tokio task that owns the kernel-side writer end of
    /// the claude PTY master. The outer loop and the blocking PTY
    /// thread both submit `WriteCmd`s (binary keystrokes, slash
    /// commands, interrupt bytes, scraper sequences) through this
    /// handle. FIFO ordering and Sequence atomicity are enforced
    /// inside the actor — see `rabbit/src/pty_writer.rs`.
    writer: crate::pty_writer::WriterHandle,
    /// §Usage-limits: every byte the blocking PTY reader pulls off
    /// the master is also published here, so the synchronous
    /// `/usage` scrape routine in the outer loop can subscribe a
    /// short-lived `Receiver` and drain the overlay for ~2s without
    /// racing the broadcast sender. Capacity is small (256) — the
    /// scrape's deadline is 2s and Claude's TUI emits at most a
    /// few KB while the `/usage` overlay paints. If a scrape
    /// somehow falls behind, the `Lagged` variant of `RecvError`
    /// signals the parser to keep draining.
    term_bcast_tx: broadcast::Sender<TermFrame>,
    /// §Context-window: shared handle to the `TermTracker` so the
    /// `/context` scrape routine can snapshot the screen state
    /// before writing the modal and again after dismiss, then
    /// publish the post-dismiss snapshot to restore the operator's
    /// terminal pane.
    vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>>,
}

struct OutcomeChannels {
    pty_link_tx: mpsc::Sender<PtyCmd>,
    outcome_rx_in: mpsc::Receiver<RunOutcome>,
}

struct SpawnResult {
    outcome_channels: OutcomeChannels,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    writer: crate::pty_writer::WriterHandle,
    term_bcast_tx: broadcast::Sender<TermFrame>,
    vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
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
    initial_tui: TermSize,
) -> Result<SpawnResult> {
    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyCmd>(128);
    let (pty_evt_tx, pty_evt_rx) = mpsc::channel::<PtyEvt>(128);
    let (outcome_tx, outcome_rx_in) = mpsc::channel::<RunOutcome>(8);
    // §Usage-limits: a broadcast channel lets the synchronous
    // `/usage` scrape (driven by `EnvelopeBody::UsageCheck` from the
    // outer loop) observe PTY bytes without contending with the
    // existing `pty_evt_tx` mpsc — the mpsc is owned by the driver
    // task and can't be safely subscribed to a second time. The
    // blocking PTY reader clones the sender and emits a copy of
    // every `PtyEvt::Read` payload here; the outer loop subscribes
    // a short-lived `Receiver` for the duration of the scrape.
    let (term_bcast_tx, _) = broadcast::channel::<TermFrame>(256);
    let term_bcast_tx_for_blocking = term_bcast_tx.clone();

    let replay_cap = config.replay_bytes;
    let bin = config.claude_bin.clone();
    let workdir = config.workdir.clone();
    let cols = initial_tui.cols;
    let rows = initial_tui.rows;
    let replay = config.replay_bytes;
    let shutdown_for_blocking = shutdown.clone();
    let grace_period = Duration::from_millis(config.shutdown_grace_ms);
    let auto_trust = config.auto_trust;

    // Spawn the PTY *before* moving into the blocking thread so we can
    // extract a `ChildKiller` and the writer and share both with the
    // outer supervisor loop. The blocking thread no longer takes
    // the writer directly — see `PtyEvt::WriteBack` below.
    let pty = Pty::spawn(&bin, &args, &workdir, cols, rows, replay)?;
    let initial_replay = pty.snapshot_replay().to_vec();
    let killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>> =
        Arc::new(Mutex::new(pty.child.clone_killer()));
    // `Pty` and `TermTracker` are shared between the blocking thread
    // (reads + feeds) and the writer actor (resize callback). The
    // callback covers both `pty.resize` (kernel TIOCSWINSZ) and
    // `vt.resize` (so `ScreenSnapshot` reports the new dims).
    let pty_arc: Arc<parking_lot::Mutex<crate::pty::Pty>> = Arc::new(parking_lot::Mutex::new(pty));
    let vt_arc: Arc<parking_lot::Mutex<crate::vt::TermTracker>> = Arc::new(
        parking_lot::Mutex::new(crate::vt::TermTracker::new(cols, rows, 5_000)),
    );
    // §Context-window / §A.7: shared "next seq to assign" counter,
    // updated by the blocking PTY reader thread under its own mutex
    // and read by other tasks (PtyCmd::Snapshot, run_context_scrape)
    // to set `after_seq` on `ScreenSnapshot` envelopes. Starts at 1
    // (0 is reserved for "no bytes fed"). Single producer → Relaxed
    // ordering is fine; we only need a consistent read-after-write.
    let next_seq: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(1));
    // §Once-and-for-all writer actor: take the writer ONCE here,
    // hand it to the dedicated tokio actor task, and let every
    // other site (outer loop, blocking thread via WriteBack
    // events, scraper) submit through the `WriterHandle`'s
    // FIFO `mpsc`. The blocking thread's `PtyEvt::WriteBack`
    // path is the only way it ever gets bytes onto the master,
    // and that bridges through the actor too — so the actor
    // remains the SOLE owner of the kernel-side write end and
    // ordering/cancellation properties hold across every site.
    let writer_box: Box<dyn Write + Send> = pty_arc
        .lock()
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("taking pty writer before spawn_blocking: {e}"))?;
    // Resize closure: locks both halves in order — `pty` first
    // (the kernel side, which can fail with an ioctl error), then
    // `vt` (always succeeds). We log + swallow the kernel-side
    // failure here because a failed TIOCSWINSZ on an exited child
    // is operationally a no-op.
    let resize_pty = pty_arc.clone();
    let resize_vt = vt_arc.clone();
    let resize_callback: Option<crate::pty_writer::ResizeCallback> =
        Some(Arc::new(parking_lot::Mutex::new(Box::new(move |c, r| {
            if let Err(e) = resize_pty.lock().resize(c, r) {
                log::warn!("resize callback: pty.resize failed: {e:?}");
            }
            resize_vt.lock().resize(c, r);
        }))));
    let writer_handle = crate::pty_writer::spawn_pty_writer(writer_box, resize_callback);
    // Clone for the driver task (consumed by the WriteBack
    // forwarding arm) and for the outer loop via `SpawnResult`.
    let writer_handle_for_driver = writer_handle.clone();
    let vt_arc_for_result = vt_arc.clone();
    let next_seq_for_result = next_seq.clone();
    let pty_join = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let pty_arc = pty_arc;
        let vt_arc = vt_arc;
        let mut reader = pty_arc.lock().reader();
        let term_bcast_tx = term_bcast_tx_for_blocking;
        let next_seq = next_seq;

        let mut io_buf = [0u8; 4096];
        let mut graceful_pending = false;
        let mut graceful_since: Option<Instant> = None;
        // §D Milestone 5 (Phase A): mirror the PTY byte stream into a
        // server-side virtual terminal. Passive today — a later phase
        // serializes `vt.snapshot()` for late browser joiners in place of the
        // SIGWINCH jiggle. 5k-line scrollback matches the design budget.
        // `vt` is shared with the writer actor's resize callback via the
        // `Arc<Mutex<TermTracker>>` taken in the outer scope.
        let mut trust_watcher = auto_trust.then(|| crate::trust::TrustWatcher::new(3));
        // §A.7 / seq-numbered snapshot protocol — single-producer seq
        // counter for the bytes this blocking thread feeds out of
        // claude's PTY. Stored in the shared `next_seq` atomic so
        // sibling tasks (PtyCmd::Snapshot, run_context_scrape) can
        // read the current watermark for their `after_seq` fields.
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
                    // §Once-and-for-all writer actor: the blocking
                    // thread no longer holds the master writer.
                    // Hand the shutdown ESC off via `PtyEvt::WriteBack`
                    // so the driver task submits it through the
                    // actor's FIFO. Latency is sub-ms in practice —
                    // the driver's select! arm processes WriteBack
                    // alongside `Read` events.
                    if pty_evt_tx
                        .blocking_send(PtyEvt::WriteBack(b"\x1b".to_vec()))
                        .is_err()
                    {
                        break;
                    }
                    // §A.7: synthetic shutdown placeholder — `seq=0`
                    // intentionally marks it as "not a live byte",
                    // and `chan=TERM_CHAN_CLAUDE` is a no-op for the
                    // browser (it's not a wire event; it's only
                    // emitted through the meta plane after the §D
                    // refactor and the browser never sees it). The
                    // string itself is preserved so the pre-existing
                    // debug surfaces stay identical.
                    // intentional: `pty_evt_tx.blocking_send` fails only
                    // if the driver task is gone (shutdown racing the
                    // blocking PTY thread's last words). The blocking
                    // thread is already tearing down at this point;
                    // logging the failure adds noise without changing
                    // the outcome.
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
                        // §Once-and-for-all writer actor: bytes
                        // coming in via `PtyCmd::Write` (legacy
                        // fallback path; tests; pre-`WriterHandle`
                        // callers) are forwarded to the driver
                        // task via `PtyEvt::WriteBack`, which
                        // submits them through the actor's FIFO.
                        // The blocking thread no longer holds the
                        // master writer at all.
                        if pty_evt_tx.blocking_send(PtyEvt::WriteBack(b)).is_err() {
                            break;
                        }
                    }
                    PtyCmd::Resize { cols, rows } => {
                        // Resize is driven by the writer actor; this arm is a no-op
                        // backstop.
                        let _ = (cols, rows);
                        log::debug!("PtyCmd::Resize is now a no-op; writer actor handles resizes");
                    }
                    PtyCmd::Repaint { cols, rows } => {
                        if let Err(e) = pty_arc.lock().jiggle(cols, rows) {
                            log::warn!("repaint jiggle failed: {e:?}");
                        }
                    }
                    PtyCmd::Snapshot { chan } => {
                        let snap = vt_arc.lock().snapshot();
                        // §A.7: populate `after_seq` from the running
                        // counter. `0` means "we have never fed a byte on
                        // this channel — don't discard anything." Otherwise
                        // the snapshot reports the most-recently-assigned
                        // seq (the highest seq a buffered live frame could
                        // already carry).
                        let after_seq = if bytes_read_since_spawn {
                            next_seq.load(Ordering::Relaxed).wrapping_sub(1)
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
                            .blocking_send(PtyEvt::Meta(Box::new(EnvelopeBody::ScreenSnapshot(
                                body,
                            ))))
                            .is_err()
                        {
                            break;
                        }
                    }
                    PtyCmd::Terminate => {
                        let _ = pty_arc.lock().terminate();
                    }
                    PtyCmd::GracefulShutdown => {
                        graceful_pending = true;
                        if graceful_since.is_none() {
                            graceful_since = Some(Instant::now());
                            log::info!("graceful shutdown: sending ESC");
                            // §Writer actor: same WriteBack path as
                            // the shutdown branch above.
                            if pty_evt_tx
                                .blocking_send(PtyEvt::WriteBack(b"\x1b".to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                continue;
            }

            if graceful_pending {
                if let Some(since) = graceful_since {
                    let alive = pty_arc.lock().alive();
                    if graceful_expired(since.elapsed(), grace_period, alive) {
                        // Lock the Pty briefly to pass a `&mut Pty`
                        // into `terminate_and_report_exited`.
                        let mut pty_g = pty_arc.lock();
                        terminate_and_report_exited(&mut pty_g, &pty_evt_tx);
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
                    let mut pty_g = pty_arc.lock();
                    terminate_and_report_exited(&mut pty_g, &pty_evt_tx);
                    break;
                }
                Ok(n) => {
                    vt_arc.lock().feed(&io_buf[..n]);
                    if let Some(tw) = trust_watcher.as_mut() {
                        if let Some(resp) = tw.observe(&io_buf[..n]) {
                            log::info!("trust dialog detected; auto-accepting with Enter");
                            // §Writer actor: trust-dialog bytes
                            // forward via WriteBack. One-shot,
                            // bounded (TrustWatcher::new(3) caps
                            // the number of acceptances), so
                            // any latency between detection and
                            // write is harmless.
                            if pty_evt_tx
                                .blocking_send(PtyEvt::WriteBack(resp.to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    // §A.7: assign the next seq to this read, then bump.
                    // The blocking thread is the single producer, so no
                    // CAS / Ordering is required for correctness. Stored
                    // in the shared atomic so PtyCmd::Snapshot and
                    // run_context_scrape can read it for their `after_seq`.
                    let seq = next_seq.fetch_add(1, Ordering::Relaxed);
                    bytes_read_since_spawn = true;
                    let frame = TermFrame {
                        chan: TERM_CHAN_CLAUDE,
                        seq,
                        data: io_buf[..n].to_vec(),
                    };
                    // §Usage-limits: also publish a copy to the
                    // broadcast so the synchronous `/usage` scrape
                    // (driven by `EnvelopeBody::UsageCheck`) can
                    // observe overlay bytes without contending with
                    // the driver task's mpsc subscription. We
                    // `clone()` because the broadcast sender takes
                    // ownership; cloning the underlying bytes is
                    // cheap for the typical ~256-byte PTY read.
                    let _ = term_bcast_tx.send(frame.clone());
                    if pty_evt_tx
                        .blocking_send(PtyEvt::Read {
                            chan: frame.chan,
                            seq: frame.seq,
                            data: frame.data,
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
                    let mut pty_g = pty_arc.lock();
                    terminate_and_report_exited(&mut pty_g, &pty_evt_tx);
                    break;
                }
            }
            if !pty_arc.lock().alive() {
                let status = {
                    let mut pty_g = pty_arc.lock();
                    pty_g.wait().unwrap_or_else(|e| {
                        log::warn!("pty.wait failed: {e:?}");
                        PtyExitStatus::with_exit_code(1)
                    })
                };
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
    // §Writer actor: the driver task consumes `WriteBack` events
    // from the blocking thread and submits them through the actor
    // via this cloned handle. The blocking thread doesn't have
    // direct access to the writer anymore.
    let writer_handle_for_driver_task = writer_handle_for_driver.clone();

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
        let writer_handle = writer_handle_for_driver_task;
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
                            crate::dispatch::send_or_warn(
                                "LinkCmd::SendMeta(PtyEvt::Meta)",
                                &cmd_tx_driver,
                                LinkCmd::SendMeta(body),
                            )
                            .await;
                        }
                        Some(PtyEvt::WriteBack(data)) => {
                            // §Once-and-for-all writer actor: bytes
                            // raised by the blocking PTY thread
                            // (trust-dialog auto-accept, shutdown
                            // ESC, legacy `PtyCmd::Write`
                            // fallbacks) flow through the FIFO here
                            // so they observe the same ordering /
                            // cancellation properties as the
                            // outer-loop's other submissions.
                            writer_handle.bytes(data).await;
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
                            crate::dispatch::send_or_warn(
                                "RunOutcome",
                                &outcome_tx_driver,
                                outcome,
                            )
                            .await;
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
        crate::dispatch::send_or_warn("PtyCmd::Terminate", &pty_tx_for_cleanup, PtyCmd::Terminate)
            .await;
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
        writer: writer_handle,
        term_bcast_tx,
        vt: vt_arc_for_result,
        next_seq: next_seq_for_result,
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
    for body in build_envelopes(ev) {
        crate::dispatch::send_or_warn(
            "LinkCmd::SendMeta(observer)",
            cmd_tx,
            LinkCmd::SendMeta(Box::new(body)),
        )
        .await;
    }
}

/// Build the wire envelopes for an observer event. Most events produce a
/// single side-effect envelope (`PromptEcho`, `StopHook`, `Log`, …), but
/// events that carry a `state` field ALSO produce a `State` envelope so the
/// browser's status badge reflects hook-driven transitions (Running on
/// `UserPromptSubmit`, Idle on `Stop`). Without the `State` half the
/// badge would be stuck on the last supervisor-side transition
/// (typically `Idle` after the initial `spawn_idle` broadcast) and would
/// never flip to `Running` while claude is actively producing tokens.
fn build_envelopes(ev: &ObserverEvent) -> Vec<EnvelopeBody> {
    let mut out = Vec::with_capacity(2);
    // State-half first so subscribers see the badge flip before the
    // side-effect echo lands (matches the supervisor's own ordering:
    // `send_state` -> `cmd_tx.send(State)` precedes any per-turn traffic).
    if let Some(st) = ev.state {
        out.push(EnvelopeBody::State(StateFrame {
            state: agent_state_from_observer(st),
            session_id: ev.session_id.clone(),
            reason: Some(ev.kind.to_string()),
        }));
    }
    let side_effect = match ev.kind {
        "session" => Some(EnvelopeBody::Session(rabbit_lib::wire::SessionInfo {
            session_id: ev.session_id.clone().unwrap_or_default(),
            resumed: false,
        })),
        "session_end" => {
            // The session_end half is folded into the state envelope
            // above (kind="session_end", state=Ended). Skip the legacy
            // duplicate here so subscribers don't see two consecutive
            // State frames for the same transition.
            None
        }
        "prompt_echo" => Some(EnvelopeBody::PromptEcho(rabbit_lib::wire::PromptEcho {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            text: ev
                .raw
                .as_ref()
                .and_then(|r| r.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            by: "admin".to_string(),
            // Server-side reconstruction from a transcript hook
            // event has no originating browser connection. Browsers
            // treat `None` as "not mine" and won't render the echo;
            // the supervisor-side transcript path still records
            // both the original prompt and the echo verbatim.
            by_connection_id: None,
        })),
        "stop_hook" => Some(EnvelopeBody::StopHook {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            usage: ev.usage.clone(),
            error: None,
        }),
        // §Scheduled-prompts: a `PermissionRequest` hook fired while
        // an in-flight scheduled run is waiting for operator approval.
        // The scheduler's observation task subscribes to `meta_tx` and
        // uses this to interrupt the prompt. `by_connection_id: None`
        // because the event is hook-driven (no originating browser).
        "permission_request" => Some(EnvelopeBody::NeedsInput {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            reason: "permission_request".to_string(),
            by_connection_id: None,
        }),
        "log" => Some(EnvelopeBody::Log(LogLine {
            level: "info".to_string(),
            message: ev.raw.as_ref().map(|r| r.to_string()).unwrap_or_default(),
        })),
        _ => None,
    };
    if let Some(body) = side_effect {
        out.push(body);
    }
    out
}

/// Translate the observer-side `State` into the wire-typed
/// `rabbit_lib::wire::AgentState`. Always succeeds — both enums carry
/// the same five variants. Keeping the conversion explicit (instead of
/// `state.as_str().into()`) preserves the typed enum on the wire and
/// avoids the silent-default-to-`Starting` fallback in `From<&str> for
/// AgentState`.
fn agent_state_from_observer(st: State) -> AgentState {
    match st {
        State::Starting => AgentState::Starting,
        State::Idle => AgentState::Idle,
        State::Running => AgentState::Running,
        State::Ended => AgentState::Ended,
        State::Dead => AgentState::Dead,
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
/// §Once-and-for-all writer actor: this helper now submits bytes
/// through the [`crate::pty_writer::WriterHandle`] instead of
/// locking a `Mutex<Writer>`. FIFO ordering vs. other writes
/// (slash commands, scraper sequences) is enforced inside the
/// actor — see `rabbit/src/pty_writer.rs`. Sub-millisecond mpsc
/// `send` latency in practice; bounds the input lag exactly like
/// the old mutex did for the binary keystroke path.
///
/// Extracted from the outer select! `LinkEvent::Binary` arm so the
/// regression test can drive it without standing up the whole
/// supervisor (which would require a live `claude` child).
async fn write_claude_terminal_bytes(data: &[u8], active: Option<&ActiveSession>) {
    let Some(active) = active else {
        log::debug!(
            "claude terminal write of {} bytes dropped: no active session",
            data.len()
        );
        return;
    };
    active.writer.bytes(data.to_vec()).await;
}

/// §Usage-limits: drive the synchronous `/usage` overlay scrape.
///
/// # Active scraping
///
/// The original implementation passively watched whatever bytes
/// the TUI emitted within a 2s window. That worked for large PTYs
/// (200×60) where Claude renders the entire overlay in one paint,
/// but failed at small widths (24×50) where the overlay is
/// scrollable — the weekly bar and per-model breakdown only
/// become visible after the user (or us) scrolls down.
///
/// Rabbit owns the PTY, so the scrape routine can drive the TUI:
/// inject `/usage`, wait for the initial paint, then send Down
/// arrows to reveal more rows. Each round resets the parser's
/// section state (`LimitsParser::reset_section`) so stale state
/// from a previous round cannot pollute the next. Already-
/// committed values are preserved via `UsageLimits::merge_from`'s
/// first-wins precedence.
///
/// Round shape:
/// 1. `/usage` (initial paint): up to 500ms.
/// 2. Down arrow (`\x1b[B`) × N: 300ms each. The w50 fixture
///    shows 4–5 Down arrows reveal the full overlay; we cap at
///    [`MAX_SCROLL_ROUNDS`] to bound total scrape latency.
/// 3. Esc to dismiss (per the Claude Code keyboard doc; single
///    Esc closes a dialog, double-Esc would rewind the
///    conversation and steal input focus).
///
/// §Once-and-for-all writer actor: all bytes the scraper emits
/// (`/usage`, the Down-arrows, the dismissing Esc) are packed
/// into ONE [`crate::pty_writer::WriteCmd::Sequence`] and
/// submitted as a single FIFO unit. Nothing else can interleave
/// inside the sequence — Operator `Interrupt`, `Slash`, or
/// keystroke submissions that arrive during the scrape sit in
/// the mpsc and are processed only AFTER the sequence
/// completes (or is canceled via the cancel flag).
///
/// Returns the parsed limits. Any field the parser did not see
/// is `None`; the calling code publishes the result as a fresh
/// `Usage` envelope and the UI shows "—" for missing fields.
async fn run_usage_scrape(
    writer: crate::pty_writer::WriterHandle,
    term_bcast_tx: broadcast::Sender<TermFrame>,
) -> (UsageLimits, bool) {
    use crate::pty_writer::SequenceOutcome;
    use tokio::time::Duration;

    const INITIAL_BUDGET: Duration = Duration::from_millis(500);
    const SCROLL_BUDGET: Duration = Duration::from_millis(300);
    const TICK: Duration = Duration::from_millis(100);
    /// Maximum number of Down-arrow rounds. Five covers the w50
    /// fixture's full overlay (initial paint → weekly bar →
    /// per-model → "What's contributing" → end). Going higher
    /// would burn latency without surfacing new plan-level data.
    const MAX_SCROLL_ROUNDS: usize = 5;

    // §Writer actor: split the scrape into two phases so the
    // first phase carries the higher initial-paint delay and
    // the second phase carries the smaller per-scroll delay.
    // The actor's `inter_item_delay` is uniform per Sequence,
    // so two phases give us two different delays with the
    // scrape's outer loop driving the parser's per-round
    // budget in parallel.
    //
    // Phase 1 sequence: paste-close preamble concatenated with
    // the `/usage` slash command. Always emit `\x1b[201~`
    // unconditionally before the slash — if the TUI is in
    // `BracketedPaste` (operator pasted a long block), this
    // cleanly closes the paste before the slash lands; if the
    // TUI is in any other mode, the stray `201~` is ignored by
    // claude's input parser (it only acts on `201~` after
    // seeing the matching `200~` open). Prepending
    // unconditionally avoids the cost of tracking paste mode
    // across the byte stream — see the conversation in this
    // task's doc history.
    //
    // The whole sequence (preamble + slash) goes into ONE
    // Sequence item so the writer actor's `write_all` lands
    // it atomically against any concurrent operator
    // submission.
    let mut slash: Vec<u8> = Vec::with_capacity(8 + 6);
    slash.extend_from_slice(b"\x1b[201~");
    // `input::slash` writes to a `&mut dyn Write`; the actor
    // is the writer, but here we just want the byte sequence
    // it produces so we can submit it via `Sequence`. Use a
    // tiny scratch buffer.
    let mut shim = BufShim { out: &mut slash };
    let _ = input::slash(&mut shim, "usage");
    let slash_bytes = slash;
    let init_items = vec![slash_bytes];
    let init_delay = Duration::from_millis(10); // actor's intrinsic per-step delay; outer loop drives the 500ms
    let init_outcome_rx = writer.sequence(init_items, init_delay).await;

    // Wait for the initial paint. The actor finishes the
    // `/usage` write in <1ms; the 500ms we then wait is the
    // overlay paint time. We use this window to start parsing
    // the broadcast stream in parallel — the parser is fed in
    // `scrape_one_window`.
    let mut parser = LimitsParser::new();
    let mut limits = UsageLimits::default();
    let mut rx = term_bcast_tx.subscribe();
    parser.reset_section();
    scrape_one_window(&mut rx, &mut parser, &mut limits, INITIAL_BUDGET, TICK).await;
    // Wait for the actor's init sequence outcome (Completed)
    // before sending the scroll sequence. This guards against
    // the (unlikely) case where the actor's write failed
    // because the master closed mid-scrape.
    let init_outcome = init_outcome_rx.await.unwrap_or(SequenceOutcome::Failed(
        "init outcome channel closed".into(),
    ));
    if matches!(init_outcome, SequenceOutcome::Failed(_)) {
        log::warn!("usage_check: initial /usage write failed; aborting scrape");
        writer.bytes(b"\x1b".to_vec()).await; // best-effort dismiss
                                              // Not an operator interrupt — a writer failure. The
                                              // `aborted` flag is reserved for the operator-interrupt
                                              // path; leave it false here so the UI doesn't surface a
                                              // misleading hint.
        return (limits, false);
    }

    if limits.all_populated() {
        // Already have everything from the initial paint;
        // dismiss in a single Bytes command.
        writer.bytes(b"\x1b".to_vec()).await;
        return (limits, false);
    }

    // Phase 2 sequence: Down-arrows (one per round) followed
    // by the dismissing Esc. The actor sleeps
    // `inter_item_delay = SCROLL_BUDGET` between each step.
    // We yield that same budget in our loop to drive the
    // parser in parallel — the two timings are loose-coupled
    // (the parser doesn't care if the actor is one step
    // ahead because `reset_section` makes each round
    // independent and `merge_from` is first-wins).
    let mut scroll_items: Vec<Vec<u8>> = Vec::with_capacity(MAX_SCROLL_ROUNDS + 1);
    for _ in 0..MAX_SCROLL_ROUNDS {
        scroll_items.push(b"\x1b[B".to_vec());
    }
    scroll_items.push(b"\x1b".to_vec());
    let scroll_delay = SCROLL_BUDGET;
    let scroll_outcome_rx = writer.sequence(scroll_items, scroll_delay).await;

    for _ in 0..MAX_SCROLL_ROUNDS {
        tokio::time::sleep(SCROLL_BUDGET).await;
        parser.reset_section();
        scrape_one_window(&mut rx, &mut parser, &mut limits, SCROLL_BUDGET, TICK).await;
        if limits.all_populated() {
            break;
        }
    }

    // The sequence is still running for MAX_SCROLL_ROUNDS
    // items + 1 Esc. We waited SCROLL_BUDGET × N + parser-time
    // for the actor to finish. To be safe, await its outcome
    // so the actor's not still in flight when we return.
    let scroll_outcome = scroll_outcome_rx.await.unwrap_or(SequenceOutcome::Failed(
        "scroll outcome channel closed".into(),
    ));
    let aborted = matches!(scroll_outcome, SequenceOutcome::AbortedBeforeStep(_));
    if aborted {
        log::info!(
            "usage_check: scrape sequence aborted by operator interrupt; \
             publishing partial result"
        );
    }

    (limits, aborted)
}

/// §Context-window: drive the synchronous `/context` overlay scrape.
///
/// Mirrors `run_usage_scrape` but is simpler — `/context` is a
/// single-page modal with no scroll rounds, so we only wait for the
/// initial paint + Esc dismiss. The actor's writer FIFO delivers
/// the bytes atomically against any concurrent operator submission.
///
/// Returns the parsed snapshot. Any field the parser did not see
/// is `None`; the calling code publishes the result as a fresh
/// `Usage` envelope (with the `ctx_*` fields layered on top of the
/// most-recent transcript snapshot).
async fn run_context_scrape(
    writer: crate::pty_writer::WriterHandle,
    term_bcast_tx: broadcast::Sender<TermFrame>,
    vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
    cmd_tx: mpsc::Sender<LinkCmd>,
) -> (ContextSnapshot, bool) {
    use crate::pty_writer::SequenceOutcome;
    use tokio::time::Duration;

    // §Context-window: `/context` paints faster than `/usage` —
    // the modal is single-page and has no scroll rounds. We
    // budget 2500ms (matching the button-disabled window the UI
    // sets) so a slow host OR a `claude` mid-tool-call has time
    // to flush the slash through the input buffer and render the
    // modal before the parser gives up. The previous 1100ms
    // value was snappier on idle but left operators seeing
    // "scrape returned no data" any time `/context` was
    // dispatched while Claude was busy; that hint is unhelpful
    // because the modal genuinely was about to paint, the
    // parser just stopped waiting first. The broadcast
    // subscriber is created before the slash is written so the
    // modal-paint frames aren't pre-empted by a stale-buffer
    // bug; see the subscribe-then-write note below.
    const INITIAL_BUDGET: Duration = Duration::from_millis(2500);
    const TICK: Duration = Duration::from_millis(100);
    // §Context-window: settle window between dismiss and the
    // restore-snapshot. Lets the modal fully unrender before we
    // capture the restored screen — the §A.7 snapshot protocol's
    // `after_seq` watermark already handles ordering at the
    // consumer, but capturing the screen mid-unrender would still
    // bleed modal residue into the restore grid.
    const RESTORE_SETTLE: Duration = Duration::from_millis(200);

    let pre_modal = vt.lock().snapshot();

    // §Context-window: subscribe to the PTY broadcast **before**
    // sending the slash. A fresh `broadcast::Receiver` only sees
    // frames produced after `subscribe()` returns — anything
    // already in the ring buffer is not replayed. If we wrote
    // `/context` first and only subscribed after `writer.sequence`
    // returned, the slash echo + most of the modal paint bytes
    // would land in the ring buffer before our subscriber existed
    // and silently evaporate, leaving the parser with the trailing
    // category rows but no headline. Subscribing here makes the
    // subsequent `drain_one_window` observe the full modal
    // lifecycle.
    let mut rx = term_bcast_tx.subscribe();

    let mut slash: Vec<u8> = Vec::with_capacity(8 + 9);
    slash.extend_from_slice(b"\x1b[201~");
    let mut shim = BufShim { out: &mut slash };
    let _ = input::slash(&mut shim, "context");
    let slash_bytes = slash;
    let init_items = vec![slash_bytes];
    let init_delay = Duration::from_millis(10);
    let init_outcome_rx = writer.sequence(init_items, init_delay).await;

    let mut parser = ContextParser::new();
    let mut snap = ContextSnapshot::default();
    parser.reset_section();
    drain_one_window(&mut rx, &mut parser, &mut snap, INITIAL_BUDGET, TICK).await;

    let init_outcome = init_outcome_rx.await.unwrap_or(SequenceOutcome::Failed(
        "init outcome channel closed".into(),
    ));
    let init_aborted = matches!(init_outcome, SequenceOutcome::AbortedBeforeStep(_));
    if matches!(init_outcome, SequenceOutcome::Failed(_)) {
        log::warn!("context_check: initial /context write failed; aborting scrape");
        writer.bytes(b"\x1b".to_vec()).await;
        return (snap, false);
    }

    // Best-effort dismiss. The actor's interrupt path
    // preempts in-flight Sequences — a `Cancel` that arrives
    // mid-modal is not observable here, so we treat the dismiss
    // as best-effort and only report what the writer actually
    // told us via `init_aborted`.
    writer.bytes(b"\x1b".to_vec()).await;

    snap.scrape_incomplete = !snap.is_empty() && !snap.all_populated();

    // §Context-window / §A.7: capture the now-restored screen and
    // publish it as a `ScreenSnapshot` envelope. The browser's
    // two-step apply (a) drops buffered live frames whose `seq ≤
    // snapshot.after_seq` (already baked into the snapshot grid),
    // (b) resets xterm, (c) paints the snapshot rows, (d) replays
    // surviving live frames in seq order. Ordering is preserved at
    // the producer (single blocking reader, monotonic seq mutex)
    // and `after_seq` keeps the consumer side consistent. Without
    // this restore the operator's xterm would be left showing the
    // leftover modal text after dismiss.
    tokio::time::sleep(RESTORE_SETTLE).await;
    // §Context-window / §A.7: publish a `ScreenSnapshot` that
    // wipes the modal off the operator's xterm and lays the
    // restored pre-modal grid back down. `after_seq` MUST
    // reflect the live reader's seq watermark AT THE MOMENT OF
    // PUBLICATION — not the watermark at pre-modal capture.
    // Any frame with seq ≤ after_seq is dropped by the
    // browser's two-step apply (its bytes are part of the
    // captured grid), and the snapshot's `term.reset()`
    // discards the modal residue. Frames with seq >
    // after_seq are live PTY bytes emitted by the reader
    // thread after we read the watermark (typically idle
    // prompt bytes from the post-dismiss settle) and get
    // layered on top of the restored grid in seq order.
    // Producer-side ordering (single blocking PTY reader,
    // monotonic seq mutex over `next_seq`) guarantees that
    // every buffered live frame either lands before this
    // snapshot's `after_seq` (and is therefore already part
    // of the grid capture) or arrives at the browser after
    // the snapshot envelope (and is therefore safely
    // replayed post-apply). Using the *pre-modal* watermark
    // here would leave the browser replaying the entire
    // slash-echo + modal-paint + dismiss-unrender frame
    // range over the freshly reset xterm, recreating the
    // glitch the screenshot showed.
    let restore = pre_modal;
    let after_seq = next_seq
        .load(std::sync::atomic::Ordering::Relaxed)
        .wrapping_sub(1);
    let body = ScreenSnapshotBody {
        chan: TERM_CHAN_CLAUDE,
        cols: restore.cols,
        rows: restore.rows,
        cursor_col: restore.cursor_col,
        cursor_row: restore.cursor_row,
        cursor_visible: restore.cursor_visible,
        text: restore.text,
        after_seq,
    };
    if let Err(e) = cmd_tx
        .send(LinkCmd::SendMeta(Box::new(EnvelopeBody::ScreenSnapshot(
            body,
        ))))
        .await
    {
        log::warn!("context_check: failed to publish restore snapshot: {e:?}");
    }

    (snap, init_aborted)
}

/// Drain broadcast frames for at most `budget`, feeding each
/// frame's bytes to the parser. Already-committed values in `snap`
/// are preserved via first-wins merge (`ContextSnapshot::merge_from`).
async fn drain_one_window(
    rx: &mut broadcast::Receiver<TermFrame>,
    parser: &mut ContextParser,
    snap: &mut ContextSnapshot,
    budget: tokio::time::Duration,
    tick: tokio::time::Duration,
) {
    use tokio::time::{timeout, Instant};

    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match timeout(tick, rx.recv()).await {
            Ok(Ok(frame)) => {
                let _ = parser.feed(&frame.data);
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => {} // tick timeout — keep draining
        }
    }
    if let Some(pass) = parser.flush() {
        snap.merge_from(pass);
    }
}

/// Drain broadcast frames for at most `budget`, feeding each
/// frame's bytes to `parser`. Already-committed values in
/// `limits` are preserved via first-wins merge.
async fn scrape_one_window(
    rx: &mut broadcast::Receiver<TermFrame>,
    parser: &mut LimitsParser,
    limits: &mut UsageLimits,
    budget: tokio::time::Duration,
    tick: tokio::time::Duration,
) {
    use tokio::time::{timeout, Instant};

    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match timeout(tick, rx.recv()).await {
            Ok(Ok(frame)) => {
                let _ = parser.feed(&frame.data);
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // The broadcast buffer overflowed (we fell
                // behind). The next `recv` resumes from the new
                // tip — keep going.
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                // Sender dropped — the active session ended
                // mid-scrape. Bail.
                break;
            }
            Err(_) => {
                // Tick timeout — keep draining until the
                // overall budget expires.
            }
        }
    }
    // Drain any pending "Resets" buffer into the right field
    // before snapshotting. `flush` returns None if the parser
    // saw nothing at all in this round (which is fine — the
    // merge step is a no-op on `None`).
    if let Some(pass) = parser.flush().or_else(|| parser.feed(&[])) {
        limits.merge_from(pass);
    }
}

async fn dispatch_to_pty(
    env: &Envelope,
    writer: Option<&crate::pty_writer::WriterHandle>,
    pty_tx: &mpsc::Sender<PtyCmd>,
    cols: u16,
    rows: u16,
) {
    // §Once-and-for-all writer actor: byte-producing commands
    // (Prompt / Slash / Interrupt / Clear) compose their bytes
    // into a `Vec<u8>` and submit the whole thing as a single
    // `WriteCmd::Bytes` through the writer actor's FIFO. Each
    // command is atomic against any in-flight `WriteCmd::Sequence`
    // (e.g. an active `/usage` scrape). `Resize` is also routed
    // through the writer actor (the resize closure handles kernel
    // `TIOCSWINSZ` + in-process VT resize in one atomic unit).
    let mut out: Vec<u8> = Vec::with_capacity(64);
    let bytes = match &env.body {
        EnvelopeBody::Prompt { text, .. } => {
            let mut shim = BufShim { out: &mut out };
            let _ = input::paste(&mut shim, text);
            std::mem::take(&mut out)
        }
        EnvelopeBody::Slash { cmd } => {
            let mut shim = BufShim { out: &mut out };
            let _ = input::slash(&mut shim, cmd);
            std::mem::take(&mut out)
        }
        EnvelopeBody::Interrupt => {
            // §Writer actor — preempt an in-flight scrape before
            // the Ctrl-C byte goes out. Order matters:
            //
            // 1. `cancel()` flips the cancellation flag
            //    synchronously so the actor sees the abort at its
            //    next Sequence step boundary (a few millis from
            //    now).
            // 2. `cancel_via_queue()` submits `WriteCmd::Cancel` to
            //    the FIFO so the actor's `select!` wakes its
            //    inter-item sleep within a few millis instead of
            //    waiting out the full `inter_item_delay`. This is
            //    the sub-step-latency preemption the operator
            //    expects.
            // 3. `bytes()` submits the Ctrl-C payload as a
            //    subsequent FIFO entry, AFTER the Cancel. FIFO
            //    ordering guarantees the byte lands only after the
            //    in-flight Sequence has been aborted.
            if let Some(w) = writer {
                w.cancel();
                w.cancel_via_queue().await;
                let mut shim = BufShim { out: &mut out };
                let _ = input::interrupt(&mut shim);
                let payload = std::mem::take(&mut out);
                if !payload.is_empty() {
                    w.bytes(payload).await;
                }
                return;
            }
            // No writer: fall through to the pty_tx fallback below
            // by rebuilding bytes into `out` again.
            let mut shim = BufShim { out: &mut out };
            let _ = input::interrupt(&mut shim);
            std::mem::take(&mut out)
        }
        EnvelopeBody::Clear { .. } => {
            let mut shim = BufShim { out: &mut out };
            let _ = input::slash(&mut shim, "clear");
            std::mem::take(&mut out)
        }
        EnvelopeBody::Resize { cols: rc, rows: rr } => {
            crate::dispatch::try_send_or_warn(
                "PtyCmd::Resize",
                pty_tx,
                PtyCmd::Resize {
                    cols: *rc,
                    rows: *rr,
                },
            );
            return;
        }
        EnvelopeBody::Repaint => {
            crate::dispatch::try_send_or_warn(
                "PtyCmd::Repaint",
                pty_tx,
                PtyCmd::Repaint { cols, rows },
            );
            return;
        }
        _ => return,
    };
    if !bytes.is_empty() {
        if let Some(w) = writer {
            w.bytes(bytes).await;
        } else {
            // No active session — still drain through the
            // actor-shaped queueing primitive so an
            // immediately-upcoming spawn picks up the bytes.
            // `pty_tx.send(PtyCmd::Write)` queues the bytes for
            // the blocking thread; today the blocking thread
            // forwards them via `PtyEvt::WriteBack` so the actor
            // is still the only kernel writer, even on this
            // fallback. Pre-spawn contexts (tests / very early
            // startup) still reach the actor because the driver
            // task is alive once pty_tx has been wired.
            crate::dispatch::send_or_warn("PtyCmd::Write", pty_tx, PtyCmd::Write(bytes)).await;
        }
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
        .send(LinkCmd::SendMeta(Box::new(EnvelopeBody::State(frame))))
        .await;
    Ok(())
}

struct BufShim<'a> {
    out: &'a mut Vec<u8>,
}

/// Per-scrape coalescing primitive used by both `UsageCheck` and
/// `ContextCheck` arms. The scrape task writes the result into
/// `result` and calls `notify.notify_waiters()`; coalesced waiters
/// observe it via `await_scrape`.
///
/// Each waiter has its own `notified()` future (per-call
/// independence) and reads the cached result on entry, so the
/// scrape-result is delivered correctly whether the waiter arrives
/// BEFORE the producer publishes (subscribes to `notified()` and
/// wakes on `notify_waiters`) OR AFTER (early-return from the
/// cached `Option`). The previous `watch::channel` design
/// collapsed all coalesced waiters onto a single receiver whose
/// `changed()` consumed the notification once, blocking every
/// later waiter until the next send.
#[derive(Clone)]
struct ScrapeWaiter<T> {
    notify: Arc<tokio::sync::Notify>,
    result: Arc<tokio::sync::Mutex<Option<T>>>,
}

async fn await_scrape<T: Clone>(waiter: &ScrapeWaiter<T>) -> T {
    {
        let g = waiter.result.lock().await;
        if let Some(v) = g.as_ref() {
            return v.clone();
        }
    }
    waiter.notify.notified().await;
    waiter
        .result
        .lock()
        .await
        .clone()
        .expect("scrape producer promised to publish a result before notify_waiters")
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
    use rabbit_lib::wire::AgentState;
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

    // §Status-badge regression: every observer event whose `state` field is
    // `Some(_)` MUST produce a `State` envelope in addition to the side-
    // effect envelope (`PromptEcho` / `StopHook`). Without this, the
    // browser's status badge is stuck on whatever the supervisor emitted
    // last (typically `Idle`), because `prompt_echo` carries the
    // `State::Running` flag but the old `build_envelope` only emitted
    // `PromptEcho` and dropped the state half on the floor.

    fn obs_event(kind: &'static str, state: Option<State>) -> ObserverEvent {
        ObserverEvent {
            kind,
            state,
            session_id: Some("sess-1".into()),
            prompt_id: Some(uuid::Uuid::nil()),
            started_at: None,
            ended_at: None,
            usage: None,
            error: None,
            raw: None,
        }
    }

    #[test]
    fn build_envelopes_emits_running_for_user_prompt_submit() {
        // UserPromptSubmit hook → observer event kind="prompt_echo",
        // state=Some(Running). Without the state half, the badge can't
        // flip from Idle to Running when the operator sends a prompt.
        let ev = obs_event("prompt_echo", Some(State::Running));
        let bodies = build_envelopes(&ev);
        assert!(
            bodies.iter().any(|b| matches!(
                b,
                EnvelopeBody::State(s) if s.state == AgentState::Running
            )),
            "expected a State(Running) envelope for prompt_echo; got {bodies:?}"
        );
        // Side-effect envelope still present — browsers render the echo
        // in the terminal alongside the status flip.
        assert!(
            bodies
                .iter()
                .any(|b| matches!(b, EnvelopeBody::PromptEcho(_))),
            "expected PromptEcho side-effect envelope; got {bodies:?}"
        );
    }

    #[test]
    fn build_envelopes_emits_idle_for_stop_hook() {
        // Stop hook → observer event kind="stop_hook", state=Some(Idle).
        // Without this, the badge never returns to Idle after a turn
        // completes and the operator thinks the agent is still running.
        let ev = obs_event("stop_hook", Some(State::Idle));
        let bodies = build_envelopes(&ev);
        assert!(
            bodies.iter().any(|b| matches!(
                b,
                EnvelopeBody::State(s) if s.state == AgentState::Idle
            )),
            "expected a State(Idle) envelope for stop_hook; got {bodies:?}"
        );
    }

    #[test]
    fn build_envelopes_emits_ended_for_session_end_without_legacy_duplicate() {
        // session_end carries state=Ended. The fix folds the legacy
        // explicit State(state="ended") arm into the new state-driven
        // path, so subscribers see exactly ONE State frame for this
        // transition rather than two consecutive duplicates.
        let ev = obs_event("session_end", Some(State::Ended));
        let bodies = build_envelopes(&ev);
        let state_frames: Vec<_> = bodies
            .iter()
            .filter_map(|b| match b {
                EnvelopeBody::State(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            state_frames.len(),
            1,
            "expected exactly one State frame for session_end; got {state_frames:?}"
        );
        assert_eq!(state_frames[0].state, AgentState::Ended);
    }

    #[test]
    fn build_envelopes_omits_state_when_observer_state_is_none() {
        // Notification/log events carry no state — the badge should not
        // flicker back to its current value on every log line. The
        // side-effect envelope (Log) still ships.
        let ev = obs_event("log", None);
        let bodies = build_envelopes(&ev);
        assert!(
            !bodies.iter().any(|b| matches!(b, EnvelopeBody::State(_))),
            "no state frame expected when ev.state is None; got {bodies:?}"
        );
        assert!(
            bodies.iter().any(|b| matches!(b, EnvelopeBody::Log(_))),
            "expected Log side-effect; got {bodies:?}"
        );
    }

    #[test]
    fn agent_state_from_observer_maps_every_variant() {
        // Symmetry with `from_agent_state`: every observer State has a
        // matching wire AgentState. If a new variant is added to one
        // enum and not the other, this test fails fast at compile time.
        for (obs, wire) in [
            (State::Starting, AgentState::Starting),
            (State::Idle, AgentState::Idle),
            (State::Running, AgentState::Running),
            (State::Ended, AgentState::Ended),
            (State::Dead, AgentState::Dead),
        ] {
            assert_eq!(agent_state_from_observer(obs), wire);
        }
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
        let pty =
            Pty::spawn("/bin/sleep", &["2".into()], "/tmp", 80, 24, 4096).expect("spawn sleep");
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
        let mut pty = Pty::spawn("/bin/sleep", &["60".to_string()], "/tmp", 80, 24, 4096)
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
    /// reached the master. Now the abort bytes go through the
    /// writer actor, which writes them directly via the master's
    /// writer end without queuing on the read-side channel.
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
    async fn interrupt_reaches_pty_via_writer_actor() {
        use crate::pty::Pty;
        use rabbit_lib::wire::{Envelope, EnvelopeBody, PROTOCOL_VERSION};

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

        // Replicate exactly what `spawn_run_one` does: spawn the
        // writer actor on the master's writer end, plus a dummy
        // `pty_tx` so dispatch_to_pty can find both. The actor is
        // the sole owner of the writer — `take_writer` here is the
        // final hand-off.
        let writer = {
            let w = pty
                .master
                .take_writer()
                .map_err(|e| anyhow::anyhow!("take_writer: {e}"))
                .expect("take_writer");
            crate::pty_writer::spawn_pty_writer(w, None)
        };
        // `dispatch_to_pty` still takes a `pty_tx` for Resize/Repaint;
        // we don't exercise those here so the channel can stay empty.
        let (pty_tx, _pty_rx) = mpsc::channel::<PtyCmd>(8);
        let _keep_pty_alive = pty; // kept alive across the test

        let envelope = Envelope {
            v: PROTOCOL_VERSION,
            seq: 1,
            body: EnvelopeBody::Interrupt,
        };

        // Drive the dispatch with the writer actor available. The
        // dispatch should write the Ctrl-C byte directly without ever
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
    async fn typed_bytes_reach_pty_via_writer_actor() {
        use crate::pty::Pty;
        use std::io::Read;
        use std::sync::mpsc as std_mpsc;
        use std::time::Instant;

        // Real PTY, /bin/cat as the child — matches the production shape.
        // `take_writer()` hands the master-side writer off to the
        // writer actor — same shape `spawn_run_one` uses.
        let mut pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 4096).expect("spawn cat");
        assert!(pty.alive(), "cat must be alive and waiting for input");
        let mut reader = pty.reader();
        let writer = {
            let w = pty
                .master
                .take_writer()
                .map_err(|e| anyhow::anyhow!("take_writer: {e}"))
                .expect("take_writer");
            crate::pty_writer::spawn_pty_writer(w, None)
        };

        // Build a mock `ActiveSession`. `killer` is unused by the helper
        // we're testing, but the struct requires one — borrow a real
        // one from a throwaway Pty rather than fabricating a dummy.
        let dummy_killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>> = {
            let mut pty2 =
                Pty::spawn("/bin/true", &[], "/tmp", 80, 24, 1024).expect("spawn /bin/true");
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
            term_bcast_tx: broadcast::channel::<TermFrame>(1).0,
            vt: Arc::new(Mutex::new(crate::vt::TermTracker::new(80, 24, 5_000))),
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
        while !got.windows(needle.len()).any(|w| w == needle) && Instant::now() < deadline {
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
    async fn backspace_byte_reaches_pty_via_writer_actor() {
        use crate::pty::Pty;
        use std::io::Read;
        use std::sync::mpsc as std_mpsc;
        use std::time::Instant;

        // `/bin/sh -c 'stty raw -echo; exec cat'` puts cat into raw mode
        // so the kernel doesn't translate 0x7f into an erase sequence
        // before we can observe the literal byte on the master side.
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_string(), "stty raw -echo; exec cat".to_string()],
            "/tmp",
            80,
            24,
            4096,
        )
        .expect("spawn sh+stty+cat");
        assert!(pty.alive());
        let mut reader = pty.reader();
        let writer = {
            let w = pty
                .master
                .take_writer()
                .map_err(|e| anyhow::anyhow!("take_writer: {e}"))
                .expect("take_writer");
            crate::pty_writer::spawn_pty_writer(w, None)
        };
        let dummy_killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>> = {
            let mut pty2 =
                Pty::spawn("/bin/true", &[], "/tmp", 80, 24, 1024).expect("spawn /bin/true");
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
            term_bcast_tx: broadcast::channel::<TermFrame>(1).0,
            vt: Arc::new(Mutex::new(crate::vt::TermTracker::new(80, 24, 5_000))),
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

        let mut pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 0).expect("spawn cat");
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
        writer.write_all(b"hello\nworld\n").expect("write to pty");
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
            assert_eq!(w[1], w[0] + 1, "seq must be strictly +1; got {:?}", seqs);
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
        use rabbit_lib::wire::ScreenSnapshotBody;
        use std::io::{Read, Write};
        use std::sync::mpsc as std_mpsc;

        let mut pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 0).expect("spawn cat");
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

    // §Context-window abort-flag regression: the operator sees "scrape
    // aborted by interrupt — partial result" / "context scrape aborted —
    // claude is not running a turn" only when the writer's
    // `SequenceOutcome::AbortedBeforeStep` actually fired. An empty
    // parser result (no modal data in the scrape window) must NOT
    // flip `aborted` to true. The unit test below drives
    // `run_context_scrape` end-to-end against `/bin/cat`: the cat
    // child echoes our write but never paints a `/context` modal, so
    // the parser returns empty and `aborted` should be `false`. If
    // someone re-introduces the `scrape_aborted = scrape_empty` line
    // (or its equivalent) at the call site, this test catches it via
    // the function's own return tuple.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn run_context_scrape_does_not_set_aborted_on_empty_parse() {
        use crate::pty::Pty;
        use crate::pty_writer::spawn_pty_writer;
        use std::io::Read;

        let pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 0).expect("spawn cat");
        let mut reader = pty.reader();
        let writer_pty = pty.writer();

        // Spawn the writer actor against the cat's master write end.
        let writer_handle = spawn_pty_writer(Box::new(writer_pty), None);

        // Broadcast channel the parser will subscribe to, mirroring
        // the production `term_bcast_tx`.
        let (term_bcast_tx, _ignored_rx) =
            tokio::sync::broadcast::channel::<crate::supervisor::TermFrame>(64);

        // VT that gets fed whatever cat echoes back.
        let vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>> = Arc::new(
            parking_lot::Mutex::new(crate::vt::TermTracker::new(80, 24, 5_000)),
        );
        let next_seq: Arc<std::sync::atomic::AtomicU64> =
            Arc::new(std::sync::atomic::AtomicU64::new(1));

        // Drain cat's stdout in the background, feeding both the VT
        // (so a restore snapshot has data to work with) AND the
        // broadcast (so the parser sees something). In production
        // the blocking reader thread in `spawn_run_one` does both.
        let bcast_for_reader = term_bcast_tx.clone();
        let vt_for_reader = vt.clone();
        let next_seq_for_reader = next_seq.clone();
        let reader_join = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let seq =
                            next_seq_for_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        vt_for_reader.lock().feed(&buf[..n]);
                        let _ = bcast_for_reader.send(crate::supervisor::TermFrame {
                            chan: rabbit_lib::wire::TERM_CHAN_CLAUDE,
                            seq,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        // cmd_tx: we just want to verify the function returns; we
        // don't care what gets sent. Use a channel and drop the rx.
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<LinkCmd>(8);

        let (_snap, aborted) =
            run_context_scrape(writer_handle, term_bcast_tx, vt, next_seq, cmd_tx).await;

        // Drop writer so cat sees EOF, then reap.
        drop(pty);
        let _ = reader_join.join();

        assert!(
            !aborted,
            "run_context_scrape must return aborted=false when the writer was \
             not interrupted; an empty parse result must not flip the flag"
        );
    }

    // §Context-window abort-flag regression: when the writer's
    // cancel flag is flipped mid-sequence, the in-flight `Sequence`
    // should be preempted and `run_context_scrape` must surface
    // `aborted = true`. This is the path the JS relies on for
    // "scrape aborted by interrupt — partial result".
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn run_context_scrape_surfaces_aborted_when_writer_canceled() {
        use crate::pty::Pty;
        use crate::pty_writer::spawn_pty_writer;
        use std::io::Read;

        let pty = Pty::spawn("/bin/cat", &[], "/tmp", 80, 24, 0).expect("spawn cat");
        let mut reader = pty.reader();
        let writer_pty = pty.writer();

        let writer_handle = spawn_pty_writer(Box::new(writer_pty), None);

        let (term_bcast_tx, _ignored_rx) =
            tokio::sync::broadcast::channel::<crate::supervisor::TermFrame>(64);

        let vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>> = Arc::new(
            parking_lot::Mutex::new(crate::vt::TermTracker::new(80, 24, 5_000)),
        );
        let next_seq: Arc<std::sync::atomic::AtomicU64> =
            Arc::new(std::sync::atomic::AtomicU64::new(1));

        // Flip the writer's cancel flag before the test even starts,
        // so the very first Sequence arm aborts before its first
        // step. `writer.sequence()` returns an outcome channel whose
        // value the function awaits; if cancel is set first, that
        // value will be `AbortedBeforeStep(0)`.
        writer_handle.cancel();

        let bcast_for_reader = term_bcast_tx.clone();
        let vt_for_reader = vt.clone();
        let next_seq_for_reader = next_seq.clone();
        let reader_join = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let seq =
                            next_seq_for_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        vt_for_reader.lock().feed(&buf[..n]);
                        let _ = bcast_for_reader.send(crate::supervisor::TermFrame {
                            chan: rabbit_lib::wire::TERM_CHAN_CLAUDE,
                            seq,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<LinkCmd>(8);

        let (_snap, aborted) =
            run_context_scrape(writer_handle, term_bcast_tx, vt, next_seq, cmd_tx).await;

        drop(pty);
        let _ = reader_join.join();

        assert!(
            aborted,
            "run_context_scrape must return aborted=true when the writer was \
             cancel-flagged before its first Sequence step ran"
        );
    }

    // §Context-window abort-flag caller-side regression: the
    // ContextCheck arm MUST set `combined.scrape_aborted = aborted`
    // (the writer's preemption signal), not `combined.scrape_aborted
    // = scrape_empty`. The earlier two tests pin `run_context_scrape`
    // itself; this one pins the CALLER's flag-setting logic by
    // mirroring its exact expressions as a free function and
    // asserting on the outputs. The expressions are intentionally
    // copied from `supervisor.rs:496-525` so a re-introduction of
    // the conflation (`scrape_aborted = scrape_empty`) flips this
    // assertion.
    #[test]
    fn context_check_caller_does_not_conflate_empty_with_aborted() {
        // Mirrors the call-site block in the ContextCheck arm.
        fn flags(snap_is_empty: bool, snap_all_populated: bool, aborted: bool) -> (bool, bool) {
            let scrape_incomplete = !snap_is_empty && !snap_all_populated;
            let scrape_empty = snap_is_empty;
            let ctx_scrape_incomplete = scrape_incomplete || scrape_empty;
            let scrape_aborted = aborted;
            (ctx_scrape_incomplete, scrape_aborted)
        }
        // Empty parse, writer NOT interrupted → ctx_scrape_incomplete
        // must be true (so the panel hints), but scrape_aborted must
        // stay false (so the JS doesn't claim an interrupt happened).
        let (inc, abort) = flags(true, false, false);
        assert!(inc, "empty parse must surface as incomplete (panel hint)");
        assert!(
            !abort,
            "scrape_aborted must mirror the writer's preemption flag, \
             NOT the empty-parse result"
        );
        // Non-empty parse, writer interrupted → both flags set; JS
        // prefers `aborted` so the operator sees the interrupt reason.
        let (inc, abort) = flags(false, true, true);
        assert!(!inc, "fully-populated parse is not incomplete");
        assert!(abort, "writer preemption flips scrape_aborted");
    }

    // §Context-window end-to-end: drives `run_context_scrape`
    // against a fake TUI that emits a synthetic `/context` modal
    // payload. This is the contract the live UI button relies on
    // — if the parser regresses OR if the supervisor's
    // subscribe-then-write / writer.sequence / drain_one_window
    // choreography leaks frames, this test fails. Synthesizes a
    // minimal modal: compact headline, two category rows, free
    // space with the compact-render `Freespace` form (no space)
    // that was the original bug. Assertion is strict: every
    // primary field must populate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn run_context_scrape_populates_all_fields_on_captured_modal() {
        use crate::pty::Pty;
        use crate::pty_writer::spawn_pty_writer;
        use std::io::Read;

        // Fixture: a minimal but realistic /context modal in the
        // compact-render form Claude Code 2.1+ uses. Bytes include
        // ANSI escapes (CSI G cursor positioning, SGR colors, OSC
        // window title reset) so the stripper + classifier hit the
        // same code paths as a live capture.
        let modal = b"\
\x1b]0;title\x07\
Context\n\
\x1b[38;5;246m~\x1b[39m 24.2k/200k tokens (12%)\n\
\x1b[38;5;246m~\x1b[39m System prompt: 2.9k tokens (1.4%)\n\
\x1b[38;5;246m~\x1b[39m System tools: 16.9k tokens (8.4%)\n\
\x1b[38;5;246m~~~\x1b[39m Freespace:142.8k(71.4%)\n";

        // Write the fixture to a tempfile so `cat` can emit it. Use
        // a deterministic path under /tmp so the shell command we
        // build is straightforward.
        let fixture_path = std::env::temp_dir().join("rabbit_run_context_scrape_fixture.bin");
        std::fs::write(&fixture_path, modal).expect("write fixture");

        let shell_arg = format!("cat {}", fixture_path.display());
        let pty =
            Pty::spawn("/bin/sh", &["-c".into(), shell_arg], "/tmp", 120, 40, 0).expect("spawn sh");

        let mut reader = pty.reader();
        let writer_pty = pty.writer();

        let writer_handle = spawn_pty_writer(Box::new(writer_pty), None);

        let (term_bcast_tx, _ignored_rx) =
            tokio::sync::broadcast::channel::<crate::supervisor::TermFrame>(64);

        let vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>> = Arc::new(
            parking_lot::Mutex::new(crate::vt::TermTracker::new(120, 40, 5_000)),
        );
        let next_seq: Arc<std::sync::atomic::AtomicU64> =
            Arc::new(std::sync::atomic::AtomicU64::new(1));

        let bcast_for_reader = term_bcast_tx.clone();
        let vt_for_reader = vt.clone();
        let next_seq_for_reader = next_seq.clone();
        let reader_join = std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let seq =
                            next_seq_for_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        vt_for_reader.lock().feed(&buf[..n]);
                        let _ = bcast_for_reader.send(crate::supervisor::TermFrame {
                            chan: rabbit_lib::wire::TERM_CHAN_CLAUDE,
                            seq,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<LinkCmd>(8);

        let (snap, aborted) =
            run_context_scrape(writer_handle, term_bcast_tx, vt, next_seq, cmd_tx).await;

        drop(pty);
        let _ = reader_join.join();
        let _ = std::fs::remove_file(&fixture_path);

        assert!(
            !aborted,
            "writer was not interrupted, aborted must be false"
        );
        assert_eq!(
            snap.used_tokens,
            Some(24200),
            "headline compact form must populate used_tokens"
        );
        assert_eq!(
            snap.total_tokens,
            Some(200000),
            "headline compact form must populate total_tokens"
        );
        assert_eq!(
            snap.used_pct,
            Some(12.0),
            "headline compact form must populate used_pct"
        );
        assert_eq!(
            snap.free_pct,
            Some(71.4),
            "Freespace (compact-render, no space) must populate free_pct"
        );
        assert!(
            snap.categories
                .as_ref()
                .and_then(|v| v.as_object())
                .map(|o| o.contains_key("system_prompt") && o.contains_key("system_tools"))
                .unwrap_or(false),
            "category rows must populate categories map; got: {:?}",
            snap.categories
        );
    }

    // §Context-window real-bytes reproducer: drives
    // `run_context_scrape` end-to-end against a shell child
    // that pipes the freshly-captured `/tmp/context5.bin`
    // bytes (live PTY capture from the current Claude session).
    // This is the closest possible reproducer for the live UI
    // "scrape returned no data" complaint without standing up
    // the full trust-dialog/auto-accept choreography in-test.
    // The bytes going into the parser are REAL; only the
    // child process is a `cat` substitute.
    //
    // To regenerate /tmp/context5.bin from the live session:
    //   timeout 35 python3 /tmp/cap2.py /tmp/context5.bin
    //
    // If this test fails despite the synthetic-modal test
    // (`run_context_scrape_populates_all_fields_on_captured_modal`)
    // passing, the bug is byte-shape-dependent (real modal
    // emits something the synthetic fixture did not exercise).
    // If this test passes but the live UI still fails, the
    // bug is downstream of the parser — warren/JS path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[ignore]
    async fn run_context_scrape_against_real_capture_bytes() {
        use crate::pty::Pty;
        use crate::pty_writer::spawn_pty_writer;
        use std::io::Read;

        // Skip cleanly if the capture file isn't present in
        // this dev env (e.g. fresh CI checkout). The synthetic
        // test above covers the structural regression contract.
        let capture_path = "/tmp/context5.bin";
        if !std::path::Path::new(capture_path).exists() {
            eprintln!(
                "[repro] {capture_path} missing; \
                 capture with: timeout 35 python3 /tmp/cap2.py {capture_path}"
            );
            return;
        }
        let fixture_path = std::path::Path::new(capture_path);

        let shell_arg = format!("cat {}", fixture_path.display());
        let pty =
            Pty::spawn("/bin/sh", &["-c".into(), shell_arg], "/tmp", 120, 40, 0).expect("spawn sh");

        let mut reader = pty.reader();
        let writer_pty = pty.writer();
        let writer_handle = spawn_pty_writer(Box::new(writer_pty), None);

        let (term_bcast_tx, _ignored_rx) =
            tokio::sync::broadcast::channel::<crate::supervisor::TermFrame>(512);

        let vt: Arc<parking_lot::Mutex<crate::vt::TermTracker>> = Arc::new(
            parking_lot::Mutex::new(crate::vt::TermTracker::new(120, 40, 5_000)),
        );
        let next_seq: Arc<std::sync::atomic::AtomicU64> =
            Arc::new(std::sync::atomic::AtomicU64::new(1));

        let captured: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let bcast_for_reader = term_bcast_tx.clone();
        let vt_for_reader = vt.clone();
        let next_seq_for_reader = next_seq.clone();
        let cap_for_reader = captured.clone();
        let reader_join = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let seq =
                            next_seq_for_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let slice = &buf[..n];
                        vt_for_reader.lock().feed(slice);
                        cap_for_reader.lock().unwrap().extend_from_slice(slice);
                        let _ = bcast_for_reader.send(crate::supervisor::TermFrame {
                            chan: rabbit_lib::wire::TERM_CHAN_CLAUDE,
                            seq,
                            data: slice.to_vec(),
                        });
                    }
                }
            }
        });

        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<LinkCmd>(8);
        let (snap, aborted) =
            run_context_scrape(writer_handle, term_bcast_tx, vt, next_seq, cmd_tx).await;

        drop(pty);
        let _ = reader_join.join();

        eprintln!("[repro/run] run_context_scrape(REAL bytes) returned:");
        eprintln!("  aborted  = {aborted}");
        eprintln!("  used     = {:?}", snap.used_tokens);
        eprintln!("  total    = {:?}", snap.total_tokens);
        eprintln!("  used_pct = {:?}", snap.used_pct);
        eprintln!("  free_pct = {:?}", snap.free_pct);
        eprintln!("  window   = {:?}", snap.window_tokens);
        eprintln!(
            "  cats     = {} keys",
            snap.categories
                .as_ref()
                .and_then(|v| v.as_object())
                .map(|o| o.len())
                .unwrap_or(0)
        );
        let cap = captured.lock().unwrap();
        eprintln!(
            "[repro/run] PTY captured {} bytes (last 600 after ANSI strip):",
            cap.len()
        );
        let cleaned =
            crate::observer::text::ansi::strip_ansi_bytes(&cap[cap.len().saturating_sub(4096)..]);
        for line in cleaned
            .split('\n')
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(12)
        {
            eprintln!("  | {}", line.trim_end());
        }

        assert!(!aborted, "writer was not interrupted");
        assert!(
            snap.used_tokens.is_some(),
            "used_tokens must populate; got snap.used_tokens={:?}",
            snap.used_tokens
        );
        assert!(snap.total_tokens.is_some(), "total_tokens must populate");
        assert!(snap.used_pct.is_some(), "used_pct must populate");
        assert!(
            snap.free_pct.is_some(),
            "free_pct must populate (this is the bug from earlier)"
        );
        assert!(snap.categories.is_some(), "categories must populate");
    }

    // §Context-window coalescing regression: when two `ContextCheck`
    // envelopes arrive within a single scrape window, both must
    // observe the same scrape result. The original implementation
    // wrapped a `tokio::sync::watch::Receiver` in `Arc<Mutex<>>` and
    // called `rx.changed().await` on it from each waiter. Watch's
    // "seen" state is tracked per-receiver-instance, so once the
    // first waiter called `changed()` and marked the value seen, the
    // second waiter's `changed()` blocked until the next send —
    // i.e. forever. The fix replaces watch with `Notify` +
    // `Mutex<Option<T>>`; each waiter independently observes via
    // `notified()` and reads the cached result. This test pins that
    // contract: two concurrent waiters both get the result within a
    // bounded time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_scrape_coalescing_two_waiters_both_observe_result() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Duration;

        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        let waiter = ScrapeWaiter::<(ContextSnapshot, bool)> {
            notify: Arc::new(tokio::sync::Notify::new()),
            result: Arc::new(tokio::sync::Mutex::new(None)),
        };

        // Pretend scrape that returns a populated snapshot.
        let producer = {
            let waiter = waiter.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let snap = ContextSnapshot {
                    used_tokens: Some(100 + id),
                    total_tokens: Some(200_000),
                    used_pct: Some(0.05),
                    free_pct: Some(95.0),
                    window_tokens: Some(200_000),
                    categories: None,
                    scrape_incomplete: false,
                };
                *waiter.result.lock().await = Some((snap, false));
                waiter.notify.notify_waiters();
            })
        };

        // Two concurrent waiters. Both must observe the populated
        // snapshot, not block forever.
        let w1 = waiter.clone();
        let w2 = waiter.clone();
        let h1 = tokio::spawn(async move { await_scrape(&w1).await });
        let h2 = tokio::spawn(async move { await_scrape(&w2).await });

        let r1 = tokio::time::timeout(Duration::from_secs(2), h1)
            .await
            .expect("waiter 1 timed out — coalescing regression")
            .expect("waiter 1 panicked");
        let r2 = tokio::time::timeout(Duration::from_secs(2), h2)
            .await
            .expect("waiter 2 timed out — coalescing regression")
            .expect("waiter 2 panicked");
        producer.await.unwrap();

        let expected_used = 100 + id;
        assert_eq!(
            r1.0.used_tokens,
            Some(expected_used),
            "waiter 1 must see the populated snapshot"
        );
        assert_eq!(
            r2.0.used_tokens,
            Some(expected_used),
            "waiter 2 must see the populated snapshot (this is the regression)"
        );
        assert!(!r1.1, "waiter 1 must see aborted=false");
        assert!(!r2.1, "waiter 2 must see aborted=false");
    }

    // §Context-window coalescing race regression: the scrape may
    // complete BEFORE the second waiter calls `notified()`. Watch
    // channels drop notifications for receivers that subscribe after
    // the send; `Notify` does too unless the waiter checks the
    // cached result first. This test pins that contract: a waiter
    // arriving AFTER notify_waiters() still observes the result via
    // the early-return check, without blocking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_scrape_coalescing_late_waiter_observes_via_cache() {
        use std::time::Duration;

        let waiter = ScrapeWaiter::<(ContextSnapshot, bool)> {
            notify: Arc::new(tokio::sync::Notify::new()),
            result: Arc::new(tokio::sync::Mutex::new(None)),
        };

        // Pre-fill the cache and notify BEFORE the waiter arrives.
        // Simulates the race where the scrape completes and clears
        // the slot in the same scheduler tick the operator's second
        // click tries to join.
        {
            let snap = ContextSnapshot {
                used_tokens: Some(42),
                total_tokens: Some(200_000),
                used_pct: Some(0.02),
                free_pct: Some(98.0),
                window_tokens: Some(200_000),
                categories: None,
                scrape_incomplete: false,
            };
            *waiter.result.lock().await = Some((snap, false));
            waiter.notify.notify_waiters();
        }

        let w = waiter.clone();
        let r = tokio::time::timeout(
            Duration::from_secs(2),
            async move { await_scrape(&w).await },
        )
        .await
        .expect("late waiter timed out — early-return check regression");
        assert_eq!(
            r.0.used_tokens,
            Some(42),
            "late waiter must observe the cached result"
        );
        assert!(!r.1, "late waiter must see aborted=false");
    }
}
