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
    let agent_id = prompt.agent_id;

    let handle = match state.live.registry.get(&agent_id) {
        Some(h) => h.clone(),
        None => {
            skip(&state, &prompt, "skipped_offline", None, None, None, now).await?;
            return Ok(());
        }
    };

    if handle.snapshot().state != AgentState::Idle {
        skip(&state, &prompt, "skipped_no_idle", None, None, None, now).await?;
        return Ok(());
    }

    if !prompt.ignore_inbox_state {
        let agent_model = db_ops::get_agent(&state.db, agent_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} not found"))?;
        let n = db_ops::count_inbox_for_agent(&state.db, &agent_model).await?;
        if n == 0 {
            skip(&state, &prompt, "skipped_no_inbox", None, None, None, now).await?;
            return Ok(());
        }
    }

    let (weekly_pct, session_pct) = match fetch_fresh_usage(&handle, USAGE_FETCH_TIMEOUT).await {
        Some(t) => t,
        None => {
            skip(
                &state,
                &prompt,
                "skipped_unsafe_scrape",
                None,
                None,
                None,
                now,
            )
            .await?;
            return Ok(());
        }
    };

    let weekly_i = weekly_pct.map(|x| x.round() as i32);
    let session_i = session_pct.map(|x| x.round() as i32);

    if let Some(w) = weekly_pct {
        if 100.0 - w < prompt.weekly_safety_buffer_pct as f64 {
            skip(
                &state,
                &prompt,
                "skipped_weekly_budget",
                None,
                weekly_i,
                session_i,
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
                None,
                weekly_i,
                session_i,
                now,
            )
            .await?;
            return Ok(());
        }
    }

    let prompt_id = Uuid::new_v4();
    let submit_result = handle
        .prompt_with_origin(&prompt.prompt_text, false, Uuid::nil())
        .await;
    if let Err(e) = submit_result {
        let reason = format!("prompt submit failed: {e}");
        let run = db_ops::insert_run_started(
            &state.db,
            prompt.id,
            agent_id,
            "failed",
            Some(prompt_id),
            weekly_i,
            session_i,
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
        agent_id,
        "fired",
        Some(prompt_id),
        weekly_i,
        session_i,
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

#[allow(clippy::too_many_arguments)]
async fn skip(
    state: &AppState,
    prompt: &scheduled_prompt::Model,
    outcome: &str,
    _prompt_id: Option<Uuid>,
    weekly_pct: Option<i32>,
    session_pct: Option<i32>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    db_ops::insert_run_started(
        &state.db,
        prompt.id,
        prompt.agent_id,
        outcome,
        None,
        weekly_pct,
        session_pct,
        Some(outcome),
    )
    .await?;
    let next = now + chrono::Duration::seconds(prompt.interval_seconds);
    db_ops::set_next_fire_at(&state.db, prompt.id, next, now).await?;
    log::info!(
        "scheduler: {} prompt={} agent={}",
        outcome,
        prompt.id,
        prompt.agent_id
    );
    Ok(())
}

async fn fetch_fresh_usage(
    handle: &AgentHandle,
    timeout_d: Duration,
) -> Option<(Option<f64>, Option<f64>)> {
    let mut rx = handle.subscribe_meta();
    if let Err(e) = handle.usage_check().await {
        log::error!("scheduler: usage_check send failed: {e:?}");
        return None;
    }
    let result = tokio::time::timeout(timeout_d, async {
        loop {
            match rx.recv().await {
                Ok(EnvelopeBody::Usage(snap)) => {
                    return (snap.weekly_pct, snap.session_pct);
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return (None, None);
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
