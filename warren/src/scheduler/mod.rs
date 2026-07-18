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
            db_ops::set_next_fire_at(&state.db, p.id, next, now).await?;
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
    //   - Agent scope: count of unblocked forgejo items the agent owns
    //     (assigned + unassigned-with-label per the schedule's
    //     `additional_labels`). Bypassed when `ignore_pending_forgejo_work`
    //     is set, mirroring `ignore_inbox_state` for team schedules.
    if prompt.scope == "agent" {
        if !prompt.ignore_pending_forgejo_work {
            let ((issues, prs), _errors) = crate::forgejo::count_work_items_for_agent(
                &state.db,
                agent_id,
                &prompt.additional_labels,
            )
            .await
            .unwrap_or(((0, 0), Vec::new()));
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
    let (weekly_pct, session_pct, context_pct, ctx_used_tokens) =
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
    // window's used tokens meet or exceed the schedule's threshold.
    // Absolute tokens (not a percentage) so the guardrail scales with
    // the actual model context size. Best-effort: a clear failure is
    // logged but the tick still fires — the operator configured this
    // as a guardrail, not a hard gate.
    if let Some(threshold) = prompt.context_clear_threshold_tokens {
        if threshold > 0 {
            if let Some(used) = ctx_used_tokens {
                if used >= threshold as u64 {
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
) -> Option<(Option<f64>, Option<f64>, Option<f64>, Option<u64>)> {
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
    // §Context-window: the `/usage` envelope reply carries
    // weekly/session pcts but NOT ctx_* fields. The `/context`
    // envelope reply carries ctx_used_tokens/pct. We need both for
    // the auto-clear threshold guard, so we keep reading envelopes
    // until ctx_used_tokens lands or the timeout fires. Returning on
    // the first envelope (which is almost always the `/usage` reply)
    // silently disables the threshold check because ctx_used_tokens
    // is None.
    let result = tokio::time::timeout(timeout_d, async {
        let mut weekly_pct: Option<f64> = None;
        let mut session_pct: Option<f64> = None;
        let mut ctx_used_pct: Option<f64> = None;
        let mut ctx_used_tokens: Option<u64> = None;
        loop {
            match rx.recv().await {
                Ok(EnvelopeBody::Usage(snap)) => {
                    // `/usage` envelopes never populate ctx_* (the
                    // supervisor's UsageCheck reply builds the snap
                    // with `..Default::default()`); `/context`
                    // envelopes populate only ctx_*. Merge whichever
                    // fields this envelope actually carries so a
                    // late-arriving `/context` reply fills the
                    // missing slot.
                    if snap.weekly_pct.is_some() {
                        weekly_pct = snap.weekly_pct;
                    }
                    if snap.session_pct.is_some() {
                        session_pct = snap.session_pct;
                    }
                    if snap.ctx_used_pct.is_some() {
                        ctx_used_pct = snap.ctx_used_pct;
                    }
                    if snap.ctx_used_tokens.is_some() {
                        ctx_used_tokens = snap.ctx_used_tokens;
                    }
                    // Have we seen both envelopes? The run row needs
                    // weekly/session; the auto-clear guard needs
                    // ctx_used_tokens. Bail as soon as ctx_used_tokens
                    // is in, since that's the slow envelope (the
                    // operator only sees the threshold fire on the
                    // correct value).
                    if ctx_used_tokens.is_some() {
                        return (weekly_pct, session_pct, ctx_used_pct, ctx_used_tokens);
                    }
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return (weekly_pct, session_pct, ctx_used_pct, ctx_used_tokens);
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
