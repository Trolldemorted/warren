use crate::db_ops;
use crate::entity::{scheduled_prompt, scheduled_prompt_run};
use crate::AppState;
use rabbit_lib::server::handle::AgentHandle;
use rabbit_lib::wire::{
    AgentState, EnvelopeBody, UsageSnapshot, USAGE_SOURCE_CONTEXT_CHECK, USAGE_SOURCE_USAGE_CHECK,
};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const TICK_INTERVAL: Duration = Duration::from_secs(30);
const USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Total wall-clock a stuck run will be nudged before warren gives up and
/// re-arms the schedule anyway. Four hours comfortably outlasts an
/// Anthropic weekly/session window and any plausible provider quota, so the
/// give-up should only fire on something genuinely not going to clear.
const RESUME_BUDGET: Duration = Duration::from_secs(4 * 60 * 60);

/// What a bump types into claude's PTY. Resumes the interrupted turn rather
/// than starting a new one — a fresh scheduled prompt must not fire until
/// this one has genuinely finished.
const BUMP_NUDGE: &str = "continue";

/// Delay before the next bump, or `None` once the budget is spent.
///
/// Mirrors the backoff the community watchdogs use
/// (cheapestinference/claude-auto-retry): 30/60/120/240/300s then flat 300s,
/// so a provider coming back is picked up within a couple of minutes while a
/// long outage settles into one attempt every five. ±15% jitter keeps a
/// fleet of agents from retrying in lockstep.
fn bump_delay(attempt: u32) -> Option<Duration> {
    const LADDER: [u64; 5] = [30, 60, 120, 240, 300];
    let base = LADDER
        .get(attempt as usize)
        .copied()
        .unwrap_or(*LADDER.last().unwrap());
    if Duration::from_secs(base) > RESUME_BUDGET {
        return None;
    }
    // Deterministic ±15% from the attempt number: a real random source is
    // not worth the nondeterminism in tests, and the spread across a fleet
    // is what matters.
    let spread = (base / 100 * 15) as i64;
    let offset = if spread == 0 {
        0
    } else {
        (attempt as i64 * 7919) % (2 * spread + 1) - spread
    };
    let secs = (base as i64 + offset).max(1) as u64;
    let d = Duration::from_secs(secs);
    if d > RESUME_BUDGET {
        None
    } else {
        Some(d)
    }
}
/// how long the per-run observer waits for a
/// `StopHook`/`NeedsInput`/`Dead` envelope before finalizing the run
/// as `observation_deadline` (line 727). Picked at 1 h so a long,
/// legitimate Claude turn (multi-step agentic work, large-file
/// reads, network-bound tool calls) doesn't get prematurely swept
/// while the agent is still genuinely running. The observer only
/// finalizes-with-no-status in the absence of any signal — a
/// `StopHook` arriving 59 min in still closes the run cleanly.
const OBSERVATION_HARD_DEADLINE: Duration = Duration::from_secs(3600);
/// After this many seconds without a `StopHook`/`NeedsInput`, the periodic
/// sweep presumes the run is lost and finalizes it as `'warren_restart'`.
/// Picked to comfortably exceed the longest realistic Claude turn so
/// long-running prompts aren't falsely canceled.
const STALE_RUN_THRESHOLD: Duration = Duration::from_secs(300);
const MAX_CLAIMS_PER_TICK: u64 = 64;

/// Spawn the scheduler's background tokio task. Called from
/// `run_server` after `build_router`. Returns the join handle so
/// callers can `abort()` on shutdown (not currently wired; the task
/// runs until the process exits).
pub fn spawn(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(state).await;
    })
}

async fn run(state: Arc<AppState>) {
    match reconcile_after_restart(&state).await {
        Ok(n) if n > 0 => {
            log::info!("scheduler: reconciled {n} stale run(s) after restart");
        }
        Ok(_) => {}
        Err(e) => log::error!("scheduler: restart reconciliation failed: {e:?}"),
    }

    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        ticker.tick().await;
        if let Err(e) = tick(&state).await {
            log::error!("scheduler: tick failed: {e:?}");
        }
    }
}

/// Cross-restart reconciliation scoped to runs whose supervising
/// rabbit is currently unregistered. We can't blanket-mark every
/// `outcome='fired', finished_at=NULL` row as `warren_restart`: the
/// observer task is allowed to exit via the meta-channel Closed
/// branch or the hard deadline, and those paths now write their own
/// terminal outcomes. A row still stranded in the `'fired'` state
/// after a restart implies either the run lost its supervising
/// connection (treat as `warren_restart`) or warren died mid-tick
/// before the observer could observe anything (also `warren_restart`
/// — the rabbit will have reconnected by now). A row whose agent is
/// *currently* registered means the observer is alive and writing
/// the real outcome; leave it alone.
async fn reconcile_after_restart(state: &AppState) -> anyhow::Result<u64> {
    let now = chrono::Utc::now();
    let stale =
        db_ops::list_unfinalized_runs(&state.db, now - chrono::Duration::seconds(5), 1000).await?;
    let mut reconciled: u64 = 0;
    for run in stale {
        let still_connected = run
            .agent_id
            .map(|aid| state.live.registry.contains_key(&aid))
            .unwrap_or(false);
        if still_connected {
            log::debug!(
                "scheduler: skipping stale run {} on reconcile (prompt={}, agent still registered)",
                run.id,
                run.scheduled_prompt_id
            );
            continue;
        }
        db_ops::finalize_run(&state.db, run.id, "warren_restart", Some("warren_restart")).await?;
        if let Some(p) = db_ops::get_scheduled_prompt(&state.db, run.scheduled_prompt_id).await? {
            let next = now + chrono::Duration::seconds(p.interval_seconds);
            db_ops::reschedule_next_fire(&state.db, p.id, next).await?;
            db_ops::mark_scheduled_prompt_finished(&state.db, p.id, now).await?;
        }
        reconciled += 1;
    }
    Ok(reconciled)
}

async fn tick(state: &Arc<AppState>) -> anyhow::Result<()> {
    let now = chrono::Utc::now();

    let claimed = db_ops::claim_due_scheduled_prompts(&state.db, now, MAX_CLAIMS_PER_TICK).await?;
    for prompt in claimed {
        let s = state.clone();
        tokio::spawn(async move {
            if let Err(e) = fire_prompt(s, prompt).await {
                log::error!("scheduler: fire_prompt failed: {e:?}");
            }
        });
    }

    let threshold = now - chrono::Duration::seconds(STALE_RUN_THRESHOLD.as_secs() as i64);
    let stale = db_ops::list_unfinalized_runs(&state.db, threshold, 100).await?;
    for run in stale {
        // Only sweep rows whose supervising rabbit isn't currently
        // registered. If the rabbit is still connected the observer
        // task is alive and the next StopHook/Dead/NeedsInput will
        // finalize the run with the right outcome; we have no signal
        // that the run is actually lost.
        let still_connected = run
            .agent_id
            .map(|aid| state.live.registry.contains_key(&aid))
            .unwrap_or(false);
        if still_connected {
            log::debug!(
                "scheduler: skipping stale run {} (prompt={}, agent still registered)",
                run.id,
                run.scheduled_prompt_id
            );
            continue;
        }
        if let Err(e) = finalize_stale_run(state, run, now).await {
            log::error!("scheduler: finalize stale run failed: {e:?}");
        }
    }

    Ok(())
}

async fn finalize_stale_run(
    state: &AppState,
    run: scheduled_prompt_run::Model,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    db_ops::finalize_run(
        &state.db,
        run.id,
        "warren_restart",
        Some("observation_sweep"),
    )
    .await?;
    db_ops::mark_scheduled_prompt_finished(&state.db, run.scheduled_prompt_id, now).await?;
    if let Some(p) = db_ops::get_scheduled_prompt(&state.db, run.scheduled_prompt_id).await? {
        let next = now + chrono::Duration::seconds(p.interval_seconds);
        db_ops::reschedule_next_fire(&state.db, p.id, next).await?;
    }
    log::warn!(
        "scheduler: stale run {} for prompt {} finalized as warren_restart",
        run.id,
        run.scheduled_prompt_id
    );
    Ok(())
}

pub async fn fire_prompt(
    state: Arc<AppState>,
    prompt: scheduled_prompt::Model,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();

    // (1) Resolve the target handle.
    //
    // Team scope: pick the first idle agent in the
    // `(target_class, target_kind)` pool. Agent scope: resolve the
    // exact agent, require it to be idle. Both branches return the
    // chosen `(agent_id, handle)` pair, or `Err(...)` after recording
    // a `skipped_no_*` run row.
    let chosen = if prompt.scope == "agent" {
        match pick_specific_agent(&state, prompt.agent_id).await? {
            Some(c) => c,
            None => {
                let outcome = match prompt.agent_id {
                    None => "skipped_no_matching_agent",
                    Some(aid) => match db_ops::get_agent(&state.db, aid).await? {
                        None => "skipped_no_matching_agent",
                        Some(_) => "skipped_no_idle_agent",
                    },
                };
                skip(&state, &prompt, outcome, (None, None, None), now).await?;
                return Ok(());
            }
        }
    } else {
        let target_class = prompt.target_class.as_deref().unwrap_or("");
        match pick_free_agent(&state, target_class, prompt.target_kind.as_deref()).await? {
            Some(c) => c,
            None => {
                let outcome = if db_ops::list_agents_by_class_kind(
                    &state.db,
                    target_class,
                    prompt.target_kind.as_deref(),
                )
                .await?
                .is_empty()
                {
                    "skipped_no_matching_agent"
                } else {
                    "skipped_no_idle_agent"
                };
                skip(&state, &prompt, outcome, (None, None, None), now).await?;
                return Ok(());
            }
        }
    };
    let (agent_id, handle) = chosen;

    // (2) Action-items gate, scoped to the schedule's address.
    //   - Team scope: warren inbox count (existing semantics).
    //   - Agent scope: count of unblocked forgejo items the agent owns
    //     (assigned + unassigned-with-label). Bypassed when
    //     `ignore_pending_forgejo_work` is set, mirroring
    //     `ignore_inbox_state` for team schedules.
    //
    // Label resolution: `prompt.additional_labels` (per-schedule
    // override set by the operator) wins first; falls back to the
    // agent's own `agents.claimable_labels`; falls back to `[class]`.
    // Same chain the agents-page Claimable column uses, so the
    // dashboard and the scheduler see the same pool.
    if prompt.scope == "agent" {
        if !prompt.ignore_pending_forgejo_work {
            // Fetch assigned + unassigned-by-label separately and sum
            // the lengths. The two helpers replace the old
            // `count_work_items_for_agent` aggregator (which returned a
            // merged count from a single config loop). Same network
            // round-trips, same per-config error swallowing, no
            // list-vs-count trade-off because we only need the count.
            let ((a_iss, a_prs), _e1) =
                crate::forgejo::assigned_work_items_for_agent(&state.db, agent_id)
                    .await
                    .unwrap_or(((Vec::new(), Vec::new()), Vec::new()));
            // Resolve the agent row once so the unclaimed call uses
            // the same labels the dashboard would show. `agent_class`
            // is the bare minimum we need; if the row's gone (race
            // with delete), the helper falls back to an empty class
            // and `unclaimed_work_items_for_agent` returns nothing,
            // which matches today's behavior.
            let (agent_claimable_labels, agent_class) =
                match db_ops::get_agent(&state.db, agent_id).await {
                    Ok(Some(m)) => (m.claimable_labels, m.class),
                    _ => (Vec::new(), String::new()),
                };
            let labels = crate::forgejo::resolve_claimable_labels(
                &prompt.additional_labels,
                &agent_claimable_labels,
                &agent_class,
            );
            let ((u_iss, u_prs), _e2) =
                crate::forgejo::unclaimed_work_items_for_agent(&state.db, agent_id, &labels)
                    .await
                    .unwrap_or(((Vec::new(), Vec::new()), Vec::new()));
            let issues = a_iss.len() + u_iss.len();
            let prs = a_prs.len() + u_prs.len();
            log::debug!(
                "scheduler: prompt={} agent={} forgejo gate labels={:?} \
                 assigned={}/{} unclaimed={}/{} -> issues={} prs={}",
                prompt.id,
                agent_id,
                labels,
                a_iss.len(),
                a_prs.len(),
                u_iss.len(),
                u_prs.len(),
                issues,
                prs
            );
            if issues + prs == 0 {
                skip(
                    &state,
                    &prompt,
                    "skipped_no_forgejo_items",
                    (None, None, None),
                    now,
                )
                .await?;
                return Ok(());
            }
        }
    } else if !prompt.ignore_inbox_state {
        let target_class = prompt.target_class.as_deref().unwrap_or("");
        let n =
            db_ops::count_inbox_by_target(&state.db, target_class, prompt.target_kind.as_deref())
                .await?;
        if n == 0 {
            skip(&state, &prompt, "skipped_no_inbox", (None, None, None), now).await?;
            return Ok(());
        }
    }

    // (3) Fresh usage scrape via the chosen handle.
    //
    // send ONLY the scrapes whose result
    // a downstream gate actually consumes. The downstream buffer
    // checks are `if let Some(_) = ...` so a None can't trip them,
    // and the optional `context_clear_threshold` needs
    // `ctx_used_tokens` to decide whether to clear. The two
    // envelopes are independent on the supervisor side (each opens
    // its own modal: `/usage` or `/context`) — there's no reason to
    // pay the visual disturbance of both modals when only one is
    // actually needed. Run row records `None` for the un-scraped
    // field, matching the "no scrape" sentinel.
    let need_usage = prompt.weekly_safety_buffer_pct > 0 || prompt.session_safety_buffer_pct > 0;
    let need_context = prompt
        .context_clear_threshold_tokens
        .map(|t| t > 0)
        .unwrap_or(false);
    let (weekly_pct, session_pct, context_pct, ctx_used_tokens) =
        match fetch_fresh_usage(&handle, USAGE_FETCH_TIMEOUT, need_usage, need_context).await {
            Some(t) => t,
            None => {
                // Failed scrape (timeout / no envelope / disconnected
                // rabbit) — block only when at least one threshold was
                // configured. Skipping the run preserves the same
                // "dependency on the scrape result" semantics the
                // pre-split version had for the weekly/session
                // thresholds.
                if missing_scrape_blocks_prompt(need_usage, need_context) {
                    // Name the guard that is blocking. `skipped_unsafe_scrape`
                    // on its own is a dead end for an operator: with a
                    // third-party model `/usage` renders nothing parseable, so
                    // a configured safety buffer can make this fire on every
                    // tick forever.
                    log::warn!(
                        "scheduler: scrape returned no usable data for prompt={} — blocking the \
                         fire. weekly_buffer={} session_buffer={} context_threshold={:?}. \
                         A threshold that the provider never satisfies will skip every tick.",
                        prompt.id,
                        prompt.weekly_safety_buffer_pct,
                        prompt.session_safety_buffer_pct,
                        prompt.context_clear_threshold_tokens
                    );
                    skip(
                        &state,
                        &prompt,
                        "skipped_unsafe_scrape",
                        (None, None, None),
                        now,
                    )
                    .await?;
                    return Ok(());
                }
                (None, None, None, None)
            }
        };
    let weekly_i = weekly_pct.map(|x| x.round() as i32);
    let session_i = session_pct.map(|x| x.round() as i32);
    // round to whole percent the same way weekly /
    // session do. A None here means the /context scrape didn't return
    // a usable envelope within the timeout window — preserve that
    // signal in the run row rather than coercing to 0.
    let context_i = context_pct.map(|x| x.round() as i32);

    if let Some(w) = weekly_pct {
        if 100.0 - w < prompt.weekly_safety_buffer_pct as f64 {
            skip(
                &state,
                &prompt,
                "skipped_weekly_budget",
                (weekly_i, session_i, context_i),
                now,
            )
            .await?;
            return Ok(());
        }
    }
    if let Some(s) = session_pct {
        if 100.0 - s < prompt.session_safety_buffer_pct as f64 {
            skip(
                &state,
                &prompt,
                "skipped_session_budget",
                (weekly_i, session_i, context_i),
                now,
            )
            .await?;
            return Ok(());
        }
    }

    // (3.5) Optional auto-`/clear` when the freshly-scraped context
    // window's used tokens meet or exceed the schedule's threshold.
    // Absolute tokens (not a percentage) so the guardrail scales with
    // the actual model context size. Best-effort: a clear failure is
    // logged but the tick still fires — the operator configured this
    // as a guardrail, not a hard gate.
    //
    // claude only treats `/clear` as a slash command when it is the
    // sole content of an input submission. If the follow-up prompt
    // bytes (step 4 below) race into the same input line, claude
    // sees the trailing `\r` from `/clear\r` as just a newline
    // before the prompt and `/clear` itself never executes. We
    // subscribe to the meta bus BEFORE issuing the clear so we don't
    // miss the `Cleared` envelope that rabbit's `dispatch_to_pty`
    // fans out after `/clear\r` is queued, then block until that
    // envelope arrives (or the deadline fires). This makes the
    // `/clear` a dedicated submission — the prompt lands only after
    // claude has had a turn to ingest `/clear` on its own.
    const AUTO_CLEAR_DEDICATED_DEADLINE: Duration = Duration::from_secs(2);
    if let Some(threshold) = prompt.context_clear_threshold_tokens {
        if threshold > 0 {
            if let Some(used) = ctx_used_tokens {
                if used >= threshold as u64 {
                    let mut cleared_rx = handle.subscribe_meta();
                    if let Err(e) = handle.clear(false).await {
                        log::warn!(
                            "scheduler: auto-clear failed for prompt={} agent={}: {e:?}",
                            prompt.id,
                            agent_id
                        );
                    }
                    let cleared_seen = tokio::time::timeout(AUTO_CLEAR_DEDICATED_DEADLINE, async {
                        loop {
                            match cleared_rx.recv().await {
                                Ok(EnvelopeBody::Cleared { .. }) => return true,
                                Ok(_) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    return false;
                                }
                            }
                        }
                    })
                    .await
                    .unwrap_or(false);
                    if !cleared_seen {
                        log::warn!(
                            "scheduler: auto-clear Cleared envelope not seen within {:?} \
                             for prompt={} agent={}; submitting prompt without dedicated-submission guarantee",
                            AUTO_CLEAR_DEDICATED_DEADLINE,
                            prompt.id,
                            agent_id,
                        );
                    }
                }
            }
        }
    }

    // (4) Submit to the chosen handle.
    let prompt_id = Uuid::new_v4();
    let submit_result = handle
        .prompt_with_origin(&prompt.prompt_text, false, Uuid::nil())
        .await;
    if let Err(e) = submit_result {
        let reason = format!("prompt submit failed: {e}");
        let run = db_ops::insert_run_started(
            &state.db,
            prompt.id,
            Some(agent_id),
            "failed",
            Some(prompt_id),
            weekly_i,
            session_i,
            context_i,
            Some(&reason),
        )
        .await?;
        log::error!(
            "scheduler: prompt submit failed for prompt={} run={}: {e:?}",
            prompt.id,
            run.id
        );
        let next = now + chrono::Duration::seconds(prompt.interval_seconds);
        db_ops::reschedule_next_fire(&state.db, prompt.id, next).await?;
        return Ok(());
    }

    let run = db_ops::insert_run_started(
        &state.db,
        prompt.id,
        Some(agent_id),
        "fired",
        Some(prompt_id),
        weekly_i,
        session_i,
        context_i,
        None,
    )
    .await?;

    log::info!(
        "scheduler: fired prompt={} agent={} prompt_id={} run={}",
        prompt.id,
        agent_id,
        prompt_id,
        run.id
    );

    spawn_observation(state.clone(), handle, prompt, run.id, now);

    Ok(())
}

/// Pick the first connected idle agent matching the given
/// `(class, kind)`. Returns `Ok(None)` when no candidate matches OR
/// when every match is offline / non-Idle. Caller distinguishes the
/// two cases via `list_agents_by_class_kind` if it needs to log the
/// distinction.
async fn pick_free_agent(
    state: &Arc<AppState>,
    class: &str,
    kind: Option<&str>,
) -> anyhow::Result<Option<(Uuid, AgentHandle)>> {
    let candidates = db_ops::list_agents_by_class_kind(&state.db, class, kind).await?;
    for a in candidates {
        if let Some(h) = state.live.registry.get(&a.id) {
            if h.snapshot().state == AgentState::Idle {
                return Ok(Some((a.id, h.clone())));
            }
        }
    }
    Ok(None)
}

/// Agent-scope variant: the address is a specific agent id. The
/// agent must (a) still exist and (b) be registered and idle right
/// now. A non-Idle registration or no registration at all returns
/// `Ok(None)` and lets the caller disambiguate via `get_agent` for
/// the `skipped_no_matching_agent` vs `skipped_no_idle_agent` log.
async fn pick_specific_agent(
    state: &Arc<AppState>,
    agent_id: Option<Uuid>,
) -> anyhow::Result<Option<(Uuid, AgentHandle)>> {
    let Some(aid) = agent_id else {
        return Ok(None);
    };
    let h = match state.live.registry.get(&aid) {
        Some(h) => h,
        None => return Ok(None),
    };
    if h.snapshot().state == AgentState::Idle {
        Ok(Some((aid, h.clone())))
    } else {
        Ok(None)
    }
}

/// Record a non-firing tick (the agent pool was empty, the inbox was
/// empty, the scrape timed out, or one of the safety budgets was
/// breached). To keep the run-history table readable, the row is
/// only inserted when the previous run for this prompt had a
/// *different* outcome — so a schedule stuck on the same skip state
/// produces one row, not one-per-tick. The schedule's `next_fire_at`
/// is always advanced so we don't spin on the same wall-clock.
async fn skip(
    state: &AppState,
    prompt: &scheduled_prompt::Model,
    outcome: &str,
    (weekly_pct, session_pct, context_pct): (Option<i32>, Option<i32>, Option<i32>),
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let prev = db_ops::list_runs_for_scheduled_prompt(&state.db, prompt.id, 1)
        .await?
        .into_iter()
        .next()
        .map(|r| r.outcome);
    // Skips used to be entirely silent — the only evidence was a run-history
    // row nobody was watching. A schedule that stops firing for hours
    // produced nothing at all in the log, which is indistinguishable from
    // "warren is idle". Log every skip, and say whether it is a repeat so
    // a persistent condition is obvious.
    if prev.as_deref() == Some(outcome) {
        log::warn!(
            "scheduler: skip prompt={} outcome={outcome} (repeating — no new run row)",
            prompt.id
        );
    } else {
        log::warn!(
            "scheduler: skip prompt={} outcome={outcome} next_fire_in={}s",
            prompt.id,
            prompt.interval_seconds
        );
    }
    if prev.as_deref() != Some(outcome) {
        db_ops::insert_run_started(
            &state.db,
            prompt.id,
            None,
            outcome,
            None,
            weekly_pct,
            session_pct,
            context_pct,
            Some(outcome),
        )
        .await?;
    }
    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
    db_ops::reschedule_next_fire(&state.db, prompt.id, next).await?;
    Ok(())
}

async fn fetch_fresh_usage(
    handle: &AgentHandle,
    timeout_d: Duration,
    need_usage: bool,
    need_context: bool,
) -> Option<(Option<f64>, Option<f64>, Option<f64>, Option<u64>)> {
    // Send the envelopes FIRST, then subscribe, so we cannot latch onto
    // an envelope that was already in flight before we asked. That alone
    // is not sufficient: the broadcast also carries `transcript` Usage
    // envelopes published throughout a turn, and the supervisor
    // back-fills cached `ctx_*` into those, so they are
    // indistinguishable from a scrape reply by shape. The merge loop
    // below therefore filters on `UsageSnapshot::source` as well — see
    // `UsageAccum`.
    if need_usage {
        if let Err(e) = handle.usage_check().await {
            log::error!("scheduler: usage_check send failed: {e:?}");
            return None;
        }
    }
    // the run-history table mirrors both /usage and
    // /context modal values. Fire `context_check` immediately after
    // `usage_check`; the supervisor coalesces if a scrape is already
    // in flight, so this is best-effort and the send-error is fine to
    // ignore — the worst case is the run row records `None` for
    // `usage_context_pct`, which matches the "no scrape yet" sentinel.
    if need_context {
        // Log the request itself. Without this, "warren asked and rabbit
        // never answered" and "warren never asked" are the same symptom —
        // neither shows up on the agent page, and the 5s wait makes them
        // indistinguishable.
        log::info!(
            "scheduler: requesting /context scrape (agent {}) for the auto-clear threshold",
            handle.agent_id
        );
        if let Err(e) = handle.context_check().await {
            // Previously ignored, on the assumption that a failed send is
            // harmless. It is not: the guard below then waits out the full
            // timeout for an envelope that was never requested, and reports
            // it as a scrape failure. Bail immediately and say so.
            log::error!("scheduler: context_check send failed, cannot evaluate the guard: {e:?}");
            return None;
        }
    }
    if !need_usage && !need_context {
        // Defensive: callers with both flags false short-circuit
        // before this fn, but if we got here, there's nothing to
        // wait for.
        return Some((None, None, None, None));
    }
    let mut rx = handle.subscribe_meta();
    // Absorb only the replies to the two envelopes we just sent. A
    // `source: "transcript"` envelope is published throughout a turn
    // and the supervisor back-fills *cached* `ctx_*` into it from the
    // previous `context_check`, so it looks exactly like a scrape
    // reply while being last tick's numbers. Latching onto one made
    // the clear/no-clear decision run on stale data — the scrape we
    // fired was then never waited for. `UsageAccum` accepts each
    // field only from the source that actually measured it.
    let result = tokio::time::timeout(timeout_d, async {
        let mut acc = UsageAccum::new(need_usage, need_context);
        loop {
            match rx.recv().await {
                Ok(EnvelopeBody::Usage(snap)) => {
                    acc.absorb(&snap);
                    if acc.satisfied() {
                        return acc.into_tuple();
                    }
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return acc.into_tuple();
                }
            }
        }
    })
    .await;
    // `result` is `Result<tuple, Elapsed>`. Timeout → return None so the
    // call site blocks on the configured threshold. A closed-channel
    // early return without the needed field is treated identically.
    let tup = match result {
        Ok(t) => t,
        Err(_) => {
            // Say we were waiting and gave up. Without this the only clue is
            // a `skipped_unsafe_scrape` row, and "warren asked but the modal
            // never painted" is indistinguishable from "warren never asked"
            // when all you can see is the agent page.
            log::warn!(
                "scheduler: usage scrape timed out after {timeout_d:?} \
                 (need_usage={need_usage}, need_context={need_context}) — \
                 no envelope carried the field the guard needs"
            );
            return None;
        }
    };
    let (weekly_pct, session_pct, ctx_used_pct, ctx_used_tokens) = tup;
    if !usage_collected(need_usage, weekly_pct) || !context_collected(need_context, ctx_used_tokens)
    {
        log::warn!(
            "scheduler: usage scrape closed without the required field \
             (need_usage={need_usage} got_weekly={weekly_pct:?}, need_context={need_context} \
             got_ctx_used_tokens={ctx_used_tokens:?}) — the /usage or /context modal \
             did not deliver a parseable reply"
        );
        return None;
    }
    Some((weekly_pct, session_pct, ctx_used_pct, ctx_used_tokens))
}

fn usage_collected(need_usage: bool, weekly_pct: Option<f64>) -> bool {
    !need_usage || weekly_pct.is_some()
}

fn context_collected(need_context: bool, ctx_used_tokens: Option<u64>) -> bool {
    !need_context || ctx_used_tokens.is_some()
}

/// Folds `Usage` envelopes into the one set of numbers this scrape asked
/// for, refusing any field that did not come from the envelope that
/// measured it.
///
/// The two scrapes are disjoint: a `usage_check` reply carries the
/// plan-level weekly/session limits and no `ctx_*`; a `context_check`
/// reply carries `ctx_*` (and echoes cached limits, which we ignore —
/// we already have the authoritative ones from `usage_check`). A
/// `transcript` envelope carries neither freshly: its limits are
/// parsed from the transcript and its `ctx_*` are back-filled from the
/// previous scrape.
struct UsageAccum {
    need_usage: bool,
    need_context: bool,
    weekly_pct: Option<f64>,
    session_pct: Option<f64>,
    ctx_used_pct: Option<f64>,
    ctx_used_tokens: Option<u64>,
}

impl UsageAccum {
    fn new(need_usage: bool, need_context: bool) -> Self {
        Self {
            need_usage,
            need_context,
            weekly_pct: None,
            session_pct: None,
            ctx_used_pct: None,
            ctx_used_tokens: None,
        }
    }

    fn absorb(&mut self, snap: &UsageSnapshot) {
        if snap.source == USAGE_SOURCE_USAGE_CHECK {
            if snap.weekly_pct.is_some() {
                self.weekly_pct = snap.weekly_pct;
            }
            if snap.session_pct.is_some() {
                self.session_pct = snap.session_pct;
            }
        }
        if snap.source == USAGE_SOURCE_CONTEXT_CHECK {
            if snap.ctx_used_tokens.is_some() {
                self.ctx_used_tokens = snap.ctx_used_tokens;
            }
            if snap.ctx_used_pct.is_some() {
                self.ctx_used_pct = snap.ctx_used_pct;
            }
        }
    }

    fn satisfied(&self) -> bool {
        usage_collected(self.need_usage, self.weekly_pct)
            && context_collected(self.need_context, self.ctx_used_tokens)
    }

    fn into_tuple(self) -> (Option<f64>, Option<f64>, Option<f64>, Option<u64>) {
        (
            self.weekly_pct,
            self.session_pct,
            self.ctx_used_pct,
            self.ctx_used_tokens,
        )
    }
}

/// Close a run out and put the schedule back on its normal cadence.
///
/// Every terminal path writes the same three things — the run outcome, the
/// finish marker that anchors the next interval, and `next_fire_at` — and
/// getting the order wrong leaves the schedule silently wedged. One helper
/// keeps them together.
async fn finalize_and_rearm(
    state: &AppState,
    prompt: &scheduled_prompt::Model,
    run_id: Uuid,
    fired_at: chrono::DateTime<chrono::Utc>,
    outcome: &str,
    error: Option<&str>,
) {
    let now = chrono::Utc::now();
    if let Err(e) = db_ops::finalize_run(&state.db, run_id, outcome, error).await {
        log::error!("scheduler: finalize {outcome} failed: {e:?}");
    }
    if let Err(e) = db_ops::mark_scheduled_prompt_finished(&state.db, prompt.id, now).await {
        log::error!("scheduler: mark finished failed: {e:?}");
    }
    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
    if let Err(e) = db_ops::set_next_fire_at(&state.db, prompt.id, next, fired_at).await {
        log::error!("scheduler: set_next_fire_at failed: {e:?}");
    }
}

/// A run whose turn died on an API error and is being nudged back to life.
///
/// The run row is deliberately left OPEN and `next_fire_at` left NULL, so no
/// further schedule fires until this one genuinely completes. Finalizing the
/// run would advance the schedule past work that never finished, which is the
/// behaviour the operator explicitly does not want.
struct ResumeState {
    class: String,
    kind: Option<String>,
    error_type: String,
    attempt: u32,
    waited: Duration,
}

impl ResumeState {
    /// One bump: clear a wedged input line, nudge claude to carry on, then
    /// make sure there is exactly one unclaimed bump in the agent's inbox.
    ///
    /// The nudge is the part that actually recovers the turn. An inbox row
    /// cannot: claude only acts on what reaches its PTY, and nothing claims
    /// an inbox item while the agent is stuck. The inbox row is for the
    /// scheduler's own gate — `fire_prompt`'s team-scope check is
    /// `count_inbox_by_target > 0`, so with an empty inbox the re-armed
    /// schedule would skip forever and never resume.
    ///
    /// The nudge is deliberately the word `continue`, not the schedule's own
    /// prompt text: the rule is that no *new* scheduled prompt may fire
    /// until the interrupted one truly finishes, and `continue` resumes the
    /// work already in flight instead of starting something new. This is
    /// also what the community watchdogs type.
    async fn bump(&self, state: &AppState, handle: &AgentHandle) {
        let mut ready = false;
        match handle.state().await {
            Ok(snapshot) if snapshot.state != rabbit_lib::wire::AgentState::Idle => {
                // Only act on an idle session; a working one is making
                // progress and must not be interrupted. This is the
                // safeguard the community watchdogs all converge on.
                if let Err(e) = handle.interrupt().await {
                    log::warn!("scheduler: resume interrupt failed: {e:?}");
                } else {
                    log::info!(
                        "scheduler: resume interrupted a non-idle agent ({:?}) to clear wedged input",
                        snapshot.state
                    );
                }
                // Give the interrupt a moment to land, otherwise the nudge
                // races the Ctrl-C and the busy-gate rejects it.
                tokio::time::sleep(Duration::from_millis(750)).await;
                ready = handle
                    .state()
                    .await
                    .map(|s| s.state == rabbit_lib::wire::AgentState::Idle)
                    .unwrap_or(false);
            }
            Ok(_) => ready = true,
            Err(e) => log::warn!("scheduler: resume could not read agent state: {e:?}"),
        }
        if ready {
            match handle
                .prompt_with_origin(BUMP_NUDGE, false, uuid::Uuid::nil())
                .await
            {
                Ok(_) => log::info!(
                    "scheduler: nudged class={} to continue after {}",
                    self.class,
                    self.error_type
                ),
                Err(e) => log::warn!("scheduler: resume nudge rejected: {e:?}"),
            }
        } else {
            log::warn!(
                "scheduler: agent still not idle after interrupt — skipping nudge, \
                 will retry next cycle"
            );
        }
        let payload = format!(
            "scheduled prompt `{}` was interrupted by an API error ({}); \
             this is a scheduler bump, safe to claim once you are running again",
            self.class, self.error_type
        );
        match db_ops::ensure_scheduler_bump(&state.db, &self.class, self.kind.as_deref(), &payload)
            .await
        {
            Ok(Some(_)) => log::info!(
                "scheduler: queued inbox bump for class={} after {}",
                self.class,
                self.error_type
            ),
            Ok(None) => {}
            Err(e) => log::warn!("scheduler: inbox bump failed: {e:?}"),
        }
    }
}

fn spawn_observation(
    state: Arc<AppState>,
    handle: AgentHandle,
    prompt: scheduled_prompt::Model,
    run_id: Uuid,
    fired_at: chrono::DateTime<chrono::Utc>,
) {
    tokio::spawn(async move {
        observe(state, handle, prompt, run_id, fired_at).await;
    });
}

async fn observe(
    state: Arc<AppState>,
    handle: AgentHandle,
    prompt: scheduled_prompt::Model,
    run_id: Uuid,
    fired_at: chrono::DateTime<chrono::Utc>,
) {
    let mut rx = handle.subscribe_meta();
    let deadline = tokio::time::sleep(OBSERVATION_HARD_DEADLINE);
    tokio::pin!(deadline);
    // Parked far in the future and only armed while a run is in resume
    // mode, so the `if resuming` guard keeps the branch disabled otherwise.
    let bump_tick = tokio::time::sleep(Duration::from_secs(86_400));
    tokio::pin!(bump_tick);
    let mut resuming: Option<ResumeState> = None;

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(EnvelopeBody::StopHook { error, .. }) => {
                    let now = chrono::Utc::now();
                    let outcome = if error.is_some() { "completed_error" } else { "completed" };
                    if let Err(e) = db_ops::finalize_run(&state.db, run_id, outcome, error.as_deref()).await {
                        log::error!("scheduler: finalize StopHook failed: {e:?}");
                    }
                    if let Err(e) = db_ops::mark_scheduled_prompt_finished(&state.db, prompt.id, now).await {
                        log::error!("scheduler: mark finished failed: {e:?}");
                    }
                    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
                    if let Err(e) = db_ops::set_next_fire_at(&state.db, prompt.id, next, fired_at).await {
                        log::error!("scheduler: set_next_fire_at failed: {e:?}");
                    }
                    log::info!(
                        "scheduler: completed prompt={} run={} outcome={}",
                        prompt.id,
                        run_id,
                        outcome
                    );
                    return;
                }
                Ok(EnvelopeBody::StopFailure { error_type, error_message }) => {
                    let class = rabbit_lib::wire::StopFailureClass::from_error_type(&error_type);
                    if class == rabbit_lib::wire::StopFailureClass::Fatal {
                        // Nothing to wait out: a billing or auth failure
                        // only the operator can fix. Finalize and re-arm
                        // so the schedule is not wedged behind a problem
                        // that will never clear.
                        finalize_and_rearm(
                            &state,
                            &prompt,
                            run_id,
                            fired_at,
                            "api_error",
                            Some(&error_message),
                        )
                        .await;
                        log::error!(
                            "scheduler: api_error (fatal, not retrying) prompt={} run={} type={error_type}: {error_message}",
                            prompt.id,
                            run_id
                        );
                        return;
                    }
                    // Retryable. The turn is still pending — we merely lost
                    // the ability to watch it. Hold the run open and the
                    // schedule suspended, and start nudging.
                    let agent = db_ops::get_agent(&state.db, handle.agent_id).await;
                    let (class, kind) = match agent {
                        Ok(Some(a)) => (a.class, a.kind),
                        _ => (String::new(), None),
                    };
                    if class.is_empty() {
                        log::error!(
                            "scheduler: cannot resume — agent {} has no class; \
                             holding run open would wedge the schedule. Finalizing.",
                            handle.agent_id
                        );
                        finalize_and_rearm(
                            &state,
                            &prompt,
                            run_id,
                            fired_at,
                            "api_error_unaddressable",
                            Some(&error_message),
                        )
                        .await;
                        return;
                    }
                    log::warn!(
                        "scheduler: api_error (retryable) prompt={} run={} type={error_type}: {error_message} \
                         — holding run open, bumping every cycle",
                        prompt.id,
                        run_id
                    );
                    let mut rs = ResumeState {
                        class,
                        kind,
                        error_type,
                        attempt: 0,
                        waited: Duration::from_secs(0),
                    };
                    // Bump immediately, then on the ladder.
                    rs.bump(&state, &handle).await;
                    rs.attempt = 1;
                    if let Some(d) = bump_delay(rs.attempt) {
                        bump_tick.as_mut().reset(tokio::time::Instant::now() + d);
                    }
                    // Push the observation hard deadline out to match the
                    // resume budget. Left alone it would fire at the 1h
                    // mark, finalize the run as `observation_deadline` and
                    // cut recovery short — reintroducing the original
                    // behaviour half way through the fix.
                    deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + RESUME_BUDGET);
                    resuming = Some(rs);
                }
                Ok(EnvelopeBody::NeedsInput { reason, .. }) => {
                    if let Err(e) = handle.interrupt().await {
                        log::error!("scheduler: interrupt on NeedsInput failed: {e:?}");
                    }
                    let now = chrono::Utc::now();
                    if let Err(e) = db_ops::finalize_run(
                        &state.db,
                        run_id,
                        "needs_input_canceled",
                        Some(&reason),
                    )
                    .await
                    {
                        log::error!("scheduler: finalize NeedsInput failed: {e:?}");
                    }
                    if let Err(e) = db_ops::mark_scheduled_prompt_finished(&state.db, prompt.id, now).await {
                        log::error!("scheduler: mark finished failed: {e:?}");
                    }
                    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
                    if let Err(e) = db_ops::set_next_fire_at(&state.db, prompt.id, next, fired_at).await {
                        log::error!("scheduler: set_next_fire_at failed: {e:?}");
                    }
                    log::info!(
                        "scheduler: needs_input_canceled prompt={} run={} reason={}",
                        prompt.id,
                        run_id,
                        reason
                    );
                    return;
                }
                Ok(EnvelopeBody::State(frame)) if frame.state == AgentState::Dead => {
                    let now = chrono::Utc::now();
                    if let Err(e) = db_ops::finalize_run(
                        &state.db,
                        run_id,
                        "rabbit_offline",
                        Some("agent went dead"),
                    )
                    .await
                    {
                        log::error!("scheduler: finalize Dead failed: {e:?}");
                    }
                    if let Err(e) = db_ops::mark_scheduled_prompt_finished(&state.db, prompt.id, now).await {
                        log::error!("scheduler: mark finished failed: {e:?}");
                    }
                    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
                    if let Err(e) = db_ops::set_next_fire_at(&state.db, prompt.id, next, fired_at).await {
                        log::error!("scheduler: set_next_fire_at failed: {e:?}");
                    }
                    log::info!(
                        "scheduler: rabbit_offline prompt={} run={}",
                        prompt.id,
                        run_id
                    );
                    return;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Channel closed means the rabbit disconnected.
                    // Finalize so the row doesn't stay at outcome='fired'
                    // and get swept as 'warren_restart' on the next boot.
                    let now = chrono::Utc::now();
                    if let Err(e) = db_ops::finalize_run(
                        &state.db,
                        run_id,
                        "meta_channel_closed",
                        Some("rabbit disconnected before StopHook"),
                    )
                    .await
                    {
                        log::error!("scheduler: finalize Closed failed: {e:?}");
                    }
                    if let Err(e) = db_ops::mark_scheduled_prompt_finished(&state.db, prompt.id, now).await {
                        log::error!("scheduler: mark finished failed: {e:?}");
                    }
                    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
                    if let Err(e) = db_ops::set_next_fire_at(&state.db, prompt.id, next, fired_at).await {
                        log::error!("scheduler: set_next_fire_at failed: {e:?}");
                    }
                    log::warn!(
                        "scheduler: meta channel closed prompt={} run={}",
                        prompt.id,
                        run_id
                    );
                    return;
                }
            },
            // Bump cycle, armed only while a run is in resume mode. Until
            // the operator's provider lets the turn finish, this is the only
            // thing keeping the run — and therefore the whole schedule —
            // alive.
            _ = &mut bump_tick, if resuming.is_some() => {
                let exhausted = {
                    let rs = resuming.as_mut().expect("guarded by `if resuming.is_some()`");
                    match bump_delay(rs.attempt) {
                        Some(d) if rs.waited + d <= RESUME_BUDGET => {
                            rs.waited += d;
                            rs.attempt += 1;
                            bump_tick
                                .as_mut()
                                .reset(tokio::time::Instant::now() + d);
                            false
                        }
                        _ => true,
                    }
                };
                if exhausted {
                    let waited = resuming.as_ref().map(|r| r.waited).unwrap_or_default();
                    finalize_and_rearm(
                        &state,
                        &prompt,
                        run_id,
                        fired_at,
                        "api_error_unrecovered",
                        Some("still failing after the resume budget elapsed"),
                    )
                    .await;
                    log::error!(
                        "scheduler: giving up on prompt={} run={} after {waited:?} of bumping",
                        prompt.id,
                        run_id
                    );
                    return;
                }
                if let Some(rs) = resuming.as_ref() {
                    rs.bump(&state, &handle).await;
                }
            }
            _ = &mut deadline => {
                // Hard deadline hit without seeing StopHook / NeedsInput /
                // Dead. The run may have completed successfully but we
                // can't observe that anymore. Finalize with a distinct
                // outcome so it doesn't get re-labeled 'warren_restart'
                // by the next sweep.
                let now = chrono::Utc::now();
                if let Err(e) = db_ops::finalize_run(
                    &state.db,
                    run_id,
                    "observation_deadline",
                    Some("observation deadline exceeded without StopHook"),
                )
                .await
                {
                    log::error!("scheduler: finalize deadline failed: {e:?}");
                }
                if let Err(e) = db_ops::mark_scheduled_prompt_finished(&state.db, prompt.id, now).await {
                    log::error!("scheduler: mark finished failed: {e:?}");
                }
                let next = now + chrono::Duration::seconds(prompt.interval_seconds);
                if let Err(e) = db_ops::set_next_fire_at(&state.db, prompt.id, next, fired_at).await {
                    log::error!("scheduler: set_next_fire_at failed: {e:?}");
                }
                log::warn!(
                    "scheduler: observation hard deadline prompt={} run={}",
                    prompt.id,
                    run_id
                );
                return;
            }
        }
    }
}

/// Pure predicate: does a missing usage scrape (timeout, no envelope,
/// disconnected rabbit) need to *block* a schedule's fire? True when
/// at least one scrape was requested for the prompt — the schedule
/// depends on the scrape result to enforce whichever threshold was
/// configured. False when no scrape was requested, i.e. no weekly/
/// session buffer AND no context-clear threshold: the schedule had
/// no reason to scrape, so a (hypothetical) failed scrape is
/// irrelevant. The two flags are computed inline at the call site;
/// this helper centralizes the boolean for the block-on-failure path.
fn missing_scrape_blocks_prompt(need_usage: bool, need_context: bool) -> bool {
    need_usage || need_context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db;
    use crate::entity::{agent, scheduled_prompt};
    use crate::rabbit_adapter;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    /// `reconcile_after_restart` must skip a stranded `'fired'` run
    /// whose supervising rabbit is still registered — the observer
    /// task is alive and writing the real outcome. Without this
    /// guard, a successful-but-delayed completion would be silently
    /// relabeled as `warren_restart` on every warren boot.
    #[tokio::test]
    async fn reconcile_skips_runs_with_registered_agents() {
        let Some(test_state) = build_test_state().await else {
            eprintln!("skipping reconcile test: DATABASE_URL not set or DB unreachable");
            return;
        };

        // 1) Seed two agent rows + a scheduled-prompt row.
        //    The agent_id on each run is what the reconcile filter
        //    checks against `live.registry`.
        let registered_agent_id = Uuid::new_v4();
        let unregistered_agent_id = Uuid::new_v4();
        let prompt_id = Uuid::new_v4();
        for aid in [registered_agent_id, unregistered_agent_id] {
            agent::ActiveModel {
                id: Set(aid),
                name: Set(format!("reconcile-test-{aid}")),
                class: Set("reconcile-test".into()),
                kind: Set(None),
                model: Set("claude".into()),
                authtoken: Set(format!("test-token-{aid}")),
                ..Default::default()
            }
            .insert(&test_state.db)
            .await
            .expect("insert agent");
        }
        scheduled_prompt::ActiveModel {
            id: Set(prompt_id),
            name: Set(format!("reconcile-test-prompt-{prompt_id}")),
            scope: Set("agent".into()),
            target_class: Set(None),
            target_kind: Set(None),
            agent_id: Set(Some(registered_agent_id)),
            prompt_text: Set("x".into()),
            interval_seconds: Set(3600),
            enabled: Set(true),
            ignore_inbox_state: Set(false),
            ignore_pending_forgejo_work: Set(false),
            weekly_safety_buffer_pct: Set(0),
            session_safety_buffer_pct: Set(0),
            context_clear_threshold_tokens: Set(None),
            additional_labels: Set(Vec::new()),
            next_fire_at: Set(Some(chrono::Utc::now() + chrono::Duration::seconds(3600))),
            last_fired_at: Set(None),
            last_finished_at: Set(None),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(&test_state.db)
        .await
        .expect("insert prompt");

        // 2) Insert one stranded run per agent. The first run points
        //    at a registered agent (must be left alone); the second
        //    points at an unregistered one (must be finalized as
        //    warren_restart). `fired_at` is backdated past the 5s
        //    `older_than` threshold that reconcile_after_restart uses
        //    to find stranded rows.
        let stranded_fired_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let registered_run_id = Uuid::new_v4();
        let unregistered_run_id = Uuid::new_v4();
        scheduled_prompt_run::ActiveModel {
            id: Set(registered_run_id),
            scheduled_prompt_id: Set(prompt_id),
            agent_id: Set(Some(registered_agent_id)),
            fired_at: Set(stranded_fired_at),
            finished_at: Set(None),
            outcome: Set("fired".into()),
            prompt_id: Set(None),
            outcome_error: Set(None),
            usage_weekly_pct: Set(None),
            usage_session_pct: Set(None),
            usage_context_pct: Set(None),
            skip_reason: Set(None),
        }
        .insert(&test_state.db)
        .await
        .expect("insert registered run");
        scheduled_prompt_run::ActiveModel {
            id: Set(unregistered_run_id),
            scheduled_prompt_id: Set(prompt_id),
            agent_id: Set(Some(unregistered_agent_id)),
            fired_at: Set(stranded_fired_at),
            finished_at: Set(None),
            outcome: Set("fired".into()),
            prompt_id: Set(None),
            outcome_error: Set(None),
            usage_weekly_pct: Set(None),
            usage_session_pct: Set(None),
            usage_context_pct: Set(None),
            skip_reason: Set(None),
        }
        .insert(&test_state.db)
        .await
        .expect("insert unregistered run");

        // 3) Register only the first agent in the live registry.
        let _handle = test_state.live.registry.register(registered_agent_id);

        // 4) Run reconcile. The exact count depends on whether other
        //    stranded rows exist in the shared test DB; we only
        //    assert on the two rows we own.
        let _reconciled = reconcile_after_restart(&test_state)
            .await
            .expect("reconcile");

        // 5) Verify outcomes.
        let rows = db_ops::list_runs_for_scheduled_prompt(&test_state.db, prompt_id, 10)
            .await
            .unwrap();
        let registered_row = rows
            .iter()
            .find(|r| r.id == registered_run_id)
            .expect("registered run row");
        let unregistered_row = rows
            .iter()
            .find(|r| r.id == unregistered_run_id)
            .expect("unregistered run row");
        assert_eq!(
            registered_row.outcome, "fired",
            "registered-agent row must remain 'fired' so the live observer can finalize it"
        );
        assert_eq!(
            unregistered_row.outcome, "warren_restart",
            "unregistered-agent row must be finalized as 'warren_restart'"
        );

        // Cleanup. Runs reference the prompt directly (no cascade),
        // so delete runs first.
        for run_id in [registered_run_id, unregistered_run_id] {
            scheduled_prompt_run::Entity::delete_by_id(run_id)
                .exec(&test_state.db)
                .await
                .expect("delete run");
        }
        scheduled_prompt::Entity::delete_by_id(prompt_id)
            .exec(&test_state.db)
            .await
            .expect("delete prompt");
        for aid in [registered_agent_id, unregistered_agent_id] {
            agent::Entity::delete_by_id(aid)
                .exec(&test_state.db)
                .await
                .expect("delete agent");
        }
    }

    #[test]
    fn missing_scrape_blocks_prompt_no_threshold_does_not_block() {
        // No thresholds set — neither envelope was requested. A
        // (hypothetical) failed scrape is irrelevant; the schedule
        // must fire.
        assert!(!missing_scrape_blocks_prompt(false, false));
    }

    #[test]
    fn missing_scrape_blocks_prompt_weekly_only_blocks() {
        // Weekly budget configured — `usage_check` was sent. Scrap
        // failure means we can't know whether we're under the budget,
        // so block.
        assert!(missing_scrape_blocks_prompt(true, false));
    }

    #[test]
    fn missing_scrape_blocks_prompt_session_only_blocks() {
        assert!(missing_scrape_blocks_prompt(true, false));
    }

    /// The bug this pins: a `transcript` Usage envelope arrives carrying
    /// *cached* `ctx_*` back-filled by the supervisor from the previous
    /// `context_check`. It has the same shape as a scrape reply, so the
    /// merge used to latch it, declare itself satisfied, and run the
    /// clear/no-clear decision on last tick's numbers — the fresh
    /// `/context` we had just fired was never waited for.
    #[test]
    fn stale_transcript_ctx_is_not_accepted_as_the_scrape_result() {
        let mut acc = UsageAccum::new(false, true);
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_TRANSCRIPT.to_string(),
            ctx_used_tokens: Some(12_000),
            ctx_used_pct: Some(0.60),
            ..Default::default()
        });
        assert!(
            !acc.satisfied(),
            "a transcript envelope must never satisfy a /context need"
        );
        assert_eq!(
            acc.ctx_used_tokens, None,
            "cached ctx_used_tokens leaked into the clear/no-clear decision"
        );
    }

    /// `/context` specifically: a `/usage` reply carries no `ctx_*` at
    /// all, so it must never stand in for the context scrape. If this
    /// were allowed through, the clear/no-clear decision would run with
    /// `ctx_used_tokens = None` (silently "no clear needed") the moment
    /// the usage scrape landed — which is the exact failure being fixed.
    #[test]
    fn usage_reply_never_substitutes_for_the_context_scrape() {
        let mut acc = UsageAccum::new(true, true);
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_USAGE_CHECK.to_string(),
            weekly_pct: Some(42.0),
            ctx_used_tokens: None,
            ..Default::default()
        });
        assert!(
            !acc.satisfied(),
            "a /usage reply must not satisfy a pending /context need"
        );
        assert!(!context_collected(true, acc.ctx_used_tokens));
    }

    /// With only a context threshold set, nothing but the `/context`
    /// reply can unblock the run. Combined with the call site's
    /// `missing_scrape_blocks_prompt` check, a missing or stale
    /// `/context` means the prompt is SKIPPED, never fired undecided.
    #[test]
    fn context_only_schedule_blocks_until_its_own_reply_lands() {
        let mut acc = UsageAccum::new(false, true);
        for src in [
            rabbit_lib::wire::USAGE_SOURCE_TRANSCRIPT,
            rabbit_lib::wire::USAGE_SOURCE_USAGE_CHECK,
        ] {
            acc.absorb(&UsageSnapshot {
                source: src.to_string(),
                weekly_pct: Some(42.0),
                ctx_used_tokens: Some(150_000),
                ..Default::default()
            });
            assert!(
                !acc.satisfied(),
                "`{src}` must not satisfy a context-only schedule"
            );
        }
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_CONTEXT_CHECK.to_string(),
            ctx_used_tokens: Some(150_000),
            ..Default::default()
        });
        assert!(acc.satisfied());
        assert_eq!(acc.ctx_used_tokens, Some(150_000));
    }

    /// The real reply must still satisfy the need.
    #[test]
    fn context_check_reply_satisfies_the_context_need() {
        let mut acc = UsageAccum::new(false, true);
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_CONTEXT_CHECK.to_string(),
            ctx_used_tokens: Some(12_000),
            ctx_used_pct: Some(0.60),
            ..Default::default()
        });
        assert!(acc.satisfied());
        assert_eq!(acc.ctx_used_tokens, Some(12_000));
    }

    /// A stale envelope must not pre-empt the real one, and the real one
    /// must win even when the stale one arrives first.
    #[test]
    fn fresh_reply_wins_when_a_stale_envelope_arrives_first() {
        let mut acc = UsageAccum::new(true, true);
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_TRANSCRIPT.to_string(),
            weekly_pct: Some(11.0),
            ctx_used_tokens: Some(1),
            ..Default::default()
        });
        assert!(!acc.satisfied());
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_USAGE_CHECK.to_string(),
            weekly_pct: Some(42.0),
            session_pct: Some(7.0),
            ..Default::default()
        });
        assert!(
            !acc.satisfied(),
            "usage alone must not satisfy a /context need"
        );
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_CONTEXT_CHECK.to_string(),
            ctx_used_tokens: Some(99_000),
            ctx_used_pct: Some(0.91),
            ..Default::default()
        });
        assert!(acc.satisfied());
        let (w, s, p, t) = acc.into_tuple();
        assert_eq!(w, Some(42.0), "must take limits from the usage_check reply");
        assert_eq!(s, Some(7.0));
        assert_eq!(
            t,
            Some(99_000),
            "must take ctx from the context_check reply"
        );
        assert_eq!(p, Some(0.91));
    }

    /// The `context_check` reply echoes cached weekly/session limits (it
    /// is built from `latest_usage()`). Those must not overwrite the
    /// authoritative ones from `usage_check`.
    #[test]
    fn context_check_reply_cannot_overwrite_fresh_limits() {
        let mut acc = UsageAccum::new(true, false);
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_USAGE_CHECK.to_string(),
            weekly_pct: Some(42.0),
            ..Default::default()
        });
        acc.absorb(&UsageSnapshot {
            source: rabbit_lib::wire::USAGE_SOURCE_CONTEXT_CHECK.to_string(),
            weekly_pct: Some(1.0),
            ..Default::default()
        });
        assert_eq!(acc.weekly_pct, Some(42.0));
    }

    /// No thresholds configured → no scrape, no envelope, and the caller
    /// must not be blocked.
    #[test]
    fn no_thresholds_needs_nothing() {
        let acc = UsageAccum::new(false, false);
        assert!(acc.satisfied(), "no thresholds means nothing to wait for");
        assert!(usage_collected(false, None));
        assert!(context_collected(false, None));
    }

    #[test]
    fn missing_scrape_blocks_prompt_context_clear_only_blocks() {
        // Operator configured only `context_clear_threshold_tokens`.
        // The auto-clear guard needs a fresh `ctx_used_tokens`
        // before it can act; a missing scrape means we can't decide
        // safely, so block.
        assert!(missing_scrape_blocks_prompt(false, true));
    }

    #[test]
    fn missing_scrape_blocks_prompt_both_thresholds_block() {
        assert!(missing_scrape_blocks_prompt(true, true));
    }

    /// `observe()` must finalize a run on any `StopHook` arriving on
    /// the agent's meta channel — not just one whose `prompt_id`
    /// matches a scheduler-minted UUID. Claude assigns its own
    /// internal `prompt_id` to the turn (`Stop` hook payload field,
    /// parsed in `rabbit/src/observer/hooks.rs`), independent of
    /// anything warren sends; the supervisor at
    /// `rabbit/src/supervisor.rs:2209` drops `Command::Prompt.id`
    /// when writing to the PTY. Before this guard was removed, every
    /// `StopHook` failed `pid == prompt_id` and the run row stayed
    /// at `outcome="fired"` indefinitely while `next_fire_at` stayed
    /// `NULL`.
    #[tokio::test]
    async fn observe_finalizes_run_when_stophook_prompt_id_differs() {
        let Some(test_state) = build_test_state().await else {
            eprintln!("skipping observe test: DATABASE_URL not set or DB unreachable");
            return;
        };

        let agent_id = Uuid::new_v4();
        let prompt_uuid = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        agent::ActiveModel {
            id: Set(agent_id),
            name: Set(format!("observe-test-{agent_id}")),
            class: Set("observe-test".into()),
            kind: Set(None),
            model: Set("claude".into()),
            authtoken: Set(format!("test-token-{agent_id}")),
            ..Default::default()
        }
        .insert(&test_state.db)
        .await
        .expect("insert agent");
        scheduled_prompt::ActiveModel {
            id: Set(prompt_uuid),
            name: Set(format!("observe-test-prompt-{prompt_uuid}")),
            scope: Set("agent".into()),
            target_class: Set(None),
            target_kind: Set(None),
            agent_id: Set(Some(agent_id)),
            prompt_text: Set("x".into()),
            interval_seconds: Set(60),
            enabled: Set(true),
            ignore_inbox_state: Set(false),
            ignore_pending_forgejo_work: Set(false),
            weekly_safety_buffer_pct: Set(0),
            session_safety_buffer_pct: Set(0),
            context_clear_threshold_tokens: Set(None),
            additional_labels: Set(Vec::new()),
            next_fire_at: Set(Some(chrono::Utc::now())),
            last_fired_at: Set(None),
            last_finished_at: Set(None),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
        }
        .insert(&test_state.db)
        .await
        .expect("insert prompt");
        scheduled_prompt_run::ActiveModel {
            id: Set(run_id),
            scheduled_prompt_id: Set(prompt_uuid),
            agent_id: Set(Some(agent_id)),
            fired_at: Set(chrono::Utc::now()),
            finished_at: Set(None),
            outcome: Set("fired".into()),
            prompt_id: Set(None),
            outcome_error: Set(None),
            usage_weekly_pct: Set(None),
            usage_session_pct: Set(None),
            usage_context_pct: Set(None),
            skip_reason: Set(None),
        }
        .insert(&test_state.db)
        .await
        .expect("insert run");

        let handle = test_state.live.registry.register(agent_id);
        let handle_for_task = handle.clone();
        let prompt_after_insert = scheduled_prompt::Entity::find_by_id(prompt_uuid)
            .one(&test_state.db)
            .await
            .unwrap()
            .unwrap();
        let fired_at = chrono::Utc::now();

        let observer_state = test_state.clone();
        let observer = tokio::spawn(async move {
            observe(
                Arc::new(observer_state),
                handle_for_task,
                prompt_after_insert,
                run_id,
                fired_at,
            )
            .await;
        });

        // observe() subscribes on its first line; give it a moment
        // before publishing so the broadcast reaches its receiver.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Publish a StopHook with a *different* prompt_id — the
        // exact bug case where Claude's UUID does not equal any
        // scheduler-minted UUID.
        handle.publish_meta(EnvelopeBody::StopHook {
            prompt_id: Uuid::new_v4(),
            usage: None,
            error: None,
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), observer)
            .await
            .expect("observe did not return after StopHook")
            .expect("observe task panicked");

        let run_after = scheduled_prompt_run::Entity::find_by_id(run_id)
            .one(&test_state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            run_after.outcome, "completed",
            "StopHook with non-matching prompt_id must still finalize the run"
        );
        assert!(
            run_after.finished_at.is_some(),
            "run.finished_at must be set after StopHook"
        );

        let prompt_after = scheduled_prompt::Entity::find_by_id(prompt_uuid)
            .one(&test_state.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            prompt_after.last_fired_at.is_some(),
            "scheduled_prompt.last_fired_at must be set"
        );
        assert!(
            prompt_after.last_finished_at.is_some(),
            "scheduled_prompt.last_finished_at must be set"
        );
        assert!(
            prompt_after.next_fire_at.is_some(),
            "scheduled_prompt.next_fire_at must be set so the schedule re-fires"
        );

        scheduled_prompt_run::Entity::delete_by_id(run_id)
            .exec(&test_state.db)
            .await
            .expect("delete run");
        scheduled_prompt::Entity::delete_by_id(prompt_uuid)
            .exec(&test_state.db)
            .await
            .expect("delete prompt");
        agent::Entity::delete_by_id(agent_id)
            .exec(&test_state.db)
            .await
            .expect("delete agent");
    }

    /// Spin up a minimal `AppState` against the test database.
    /// Spin up a minimal `AppState` against the test database.
    /// Returns `None` if `DATABASE_URL` isn't set or the DB is
    /// unreachable, so this test silently no-ops in environments
    /// without a live Postgres (CI without the test DB).
    async fn build_test_state() -> Option<AppState> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = match db::connect(&url).await {
            Ok(c) => c,
            Err(_) => return None,
        };
        let cfg = Config {
            bind_addr: "127.0.0.1:0".into(),
            database_url: url,
            admin_psk: "test-psk".into(),
            session_ttl_hours: 1,
            static_dir: Default::default(),
            docs_dir: Default::default(),
            tui_cols: 160,
            tui_rows: 50,
        };
        let live = rabbit_adapter::build_server_state(db.clone(), cfg.tui_cols, cfg.tui_rows);
        Some(AppState {
            db,
            config: cfg,
            live,
        })
    }
}

#[cfg(test)]
mod stop_failure_tests {
    use super::*;
    use rabbit_lib::wire::StopFailureClass;

    /// Fatal means "only the operator can fix this". Bumping an auth or
    /// billing failure for four hours just delays telling them, so the
    /// classification has to name exactly those.
    #[test]
    fn fatal_types_are_the_operator_fixable_ones() {
        for t in [
            "authentication_failed",
            "oauth_org_not_allowed",
            "account_on_hold",
            "billing_error",
            "invalid_request",
            "model_not_found",
        ] {
            assert_eq!(
                StopFailureClass::from_error_type(t),
                StopFailureClass::Fatal,
                "{t} must not be bumped"
            );
        }
    }

    /// Everything else is worth waiting out.
    #[test]
    fn retryable_types_cover_the_transient_ones() {
        for t in [
            "rate_limit",
            "overloaded",
            "server_error",
            "max_output_tokens",
            "cloud_credential_error",
            "unknown",
        ] {
            assert_eq!(
                StopFailureClass::from_error_type(t),
                StopFailureClass::Retryable,
                "{t} should be bumped"
            );
        }
    }

    /// A provider we have never heard of is far more likely to be a
    /// temporary rate limit than a permanent misconfiguration, and the
    /// resume budget bounds the cost of guessing wrong. Anything new is
    /// retryable.
    #[test]
    fn unknown_type_defaults_to_retryable() {
        assert_eq!(
            StopFailureClass::from_error_type("some_new_provider_error"),
            StopFailureClass::Retryable
        );
    }

    /// The ladder: 30, 60, 120, 240, 300, then flat 300 forever after.
    #[test]
    fn bump_delay_follows_the_ladder() {
        let secs = |a: u32| bump_delay(a).map(|d| d.as_secs()).unwrap_or(0);
        for (attempt, base) in [
            (0u32, 30u64),
            (1, 60),
            (2, 120),
            (3, 240),
            (4, 300),
            (9, 300),
        ] {
            let got = secs(attempt);
            // ±15% of the base value.
            let lo = (base as f64 * 0.85) as u64;
            let hi = (base as f64 * 1.15) as u64 + 1;
            assert!(
                got >= lo && got <= hi,
                "attempt {attempt}: {got}s outside the expected {lo}..={hi} band around {base}s"
            );
        }
    }

    /// The resume must give up after the 4h budget rather than nudging
    /// forever, but the budget has to outlast a real provider window.
    #[test]
    fn resume_budget_is_four_hours_and_delays_stay_inside_it() {
        assert_eq!(RESUME_BUDGET, Duration::from_secs(4 * 60 * 60));
        // Summing the flat tail of the ladder must eventually cross it.
        let mut waited = Duration::from_secs(0);
        for attempt in 0..200u32 {
            let Some(d) = bump_delay(attempt) else {
                break;
            };
            waited += d;
            if waited > RESUME_BUDGET {
                return;
            }
        }
        panic!("the ladder never exhausts the resume budget");
    }
}
