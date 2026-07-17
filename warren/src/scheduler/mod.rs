use crate::db_ops;
use crate::entity::{scheduled_prompt, scheduled_prompt_run};
use crate::AppState;
use rabbit_lib::server::handle::AgentHandle;
use rabbit_lib::wire::{AgentState, EnvelopeBody};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const TICK_INTERVAL: Duration = Duration::from_secs(30);
const USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const OBSERVATION_HARD_DEADLINE: Duration = Duration::from_secs(300);
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
    match db_ops::reconcile_after_restart(&state.db).await {
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
        db_ops::set_next_fire_at(&state.db, p.id, next, now).await?;
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
    //   - Agent scope: count of unblocked forgejo items assigned to the
    //     target agent. Bypassed when `ignore_pending_forgejo_work` is
    //     set, mirroring `ignore_inbox_state` for team schedules.
    if prompt.scope == "agent" {
        if !prompt.ignore_pending_forgejo_work {
            let (issues, prs) =
                crate::forgejo::count_unblocked_assigned_for_agent(&state.db, agent_id)
                    .await
                    .unwrap_or((0, 0));
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
    let (weekly_pct, session_pct, context_pct) =
        match fetch_fresh_usage(&handle, USAGE_FETCH_TIMEOUT).await {
            Some(t) => t,
            None => {
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
        };
    let weekly_i = weekly_pct.map(|x| x.round() as i32);
    let session_i = session_pct.map(|x| x.round() as i32);
    // §Context-window: round to whole percent the same way weekly /
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
    // window is at or above the schedule's threshold. Best-effort:
    // a clear failure is logged but the tick still fires — the
    // operator configured this as a guardrail, not a hard gate.
    if let Some(threshold) = prompt.context_clear_threshold_pct {
        if threshold > 0 {
            if let Some(c) = context_pct {
                if c >= threshold as f64 {
                    if let Err(e) = handle.clear(false).await {
                        log::warn!(
                            "scheduler: auto-clear failed for prompt={} agent={}: {e:?}",
                            prompt.id,
                            agent_id
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
        db_ops::set_next_fire_at(&state.db, prompt.id, next, now).await?;
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

    spawn_observation(state.clone(), handle, prompt, prompt_id, run.id, now);

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
    db_ops::set_next_fire_at(&state.db, prompt.id, next, now).await?;
    Ok(())
}

async fn fetch_fresh_usage(
    handle: &AgentHandle,
    timeout_d: Duration,
) -> Option<(Option<f64>, Option<f64>, Option<f64>)> {
    let mut rx = handle.subscribe_meta();
    if let Err(e) = handle.usage_check().await {
        log::error!("scheduler: usage_check send failed: {e:?}");
        return None;
    }
    // §Context-window: the run-history table mirrors both /usage and
    // /context modal values. Fire `context_check` immediately after
    // `usage_check`; the supervisor coalesces if a scrape is already
    // in flight, so this is best-effort and the send-error is fine to
    // ignore — the worst case is the run row records `None` for
    // `usage_context_pct`, which matches the "no scrape yet" sentinel.
    if let Err(e) = handle.context_check().await {
        log::warn!("scheduler: context_check send failed (ignored): {e:?}");
    }
    let result = tokio::time::timeout(timeout_d, async {
        loop {
            match rx.recv().await {
                // §Context-window: the supervisor publishes a single
                // `Usage` envelope per `context_check` scrape with
                // the `ctx_*` fields populated. We accept any Usage
                // envelope here; both `/usage` and `/context` end up
                // on the same meta channel. By the time we see the
                // first one the operator's last scrape (whichever
                // it was) has landed and we record its values. If
                // both modals fire in quick succession, the second
                // one will be a later envelope — but our caller
                // only awaits one and we accept the first match.
                Ok(EnvelopeBody::Usage(snap)) => {
                    // Prefer the most-recently-populated context pct:
                    // both scrapes land on the same channel, so the
                    // last envelope within the timeout window carries
                    // the freshest values. Capture the most recent
                    // envelope that has any of the three fields.
                    return (snap.weekly_pct, snap.session_pct, snap.ctx_used_pct);
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return (None, None, None);
                }
            }
        }
    })
    .await;
    result.ok()
}

fn spawn_observation(
    state: Arc<AppState>,
    handle: AgentHandle,
    prompt: scheduled_prompt::Model,
    prompt_id: Uuid,
    run_id: Uuid,
    fired_at: chrono::DateTime<chrono::Utc>,
) {
    tokio::spawn(async move {
        observe(state, handle, prompt, prompt_id, run_id, fired_at).await;
    });
}

async fn observe(
    state: Arc<AppState>,
    handle: AgentHandle,
    prompt: scheduled_prompt::Model,
    prompt_id: Uuid,
    run_id: Uuid,
    fired_at: chrono::DateTime<chrono::Utc>,
) {
    let mut rx = handle.subscribe_meta();
    let deadline = tokio::time::sleep(OBSERVATION_HARD_DEADLINE);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(EnvelopeBody::StopHook { prompt_id: pid, error, .. }) if pid == prompt_id => {
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
                Ok(EnvelopeBody::NeedsInput { prompt_id: pid, reason, .. }) if pid == prompt_id => {
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
                    log::warn!(
                        "scheduler: meta channel closed for prompt={}",
                        prompt.id
                    );
                    return;
                }
            },
            _ = &mut deadline => {
                log::warn!(
                    "scheduler: observation hard deadline for prompt={} run={}",
                    prompt.id,
                    run_id
                );
                return;
            }
        }
    }
}
