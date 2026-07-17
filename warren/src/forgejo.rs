use chrono::{DateTime, TimeZone, Utc};
use forgejo_api::structs::{Issue, IssueListIssuesQuery, IssueListIssuesQueryState, StateType};
use forgejo_api::{Auth, Forgejo};
use url::Url;
use uuid::Uuid;

use crate::db::Db;
use crate::error::{AppError, AppResult};
use crate::models::ActionItem;

fn client(base_url: &str, access_token: &str) -> AppResult<Forgejo> {
    let url = Url::parse(base_url)
        .map_err(|e| AppError::BadRequest(format!("invalid forgejo base_url: {e}")))?;
    Forgejo::with_user_agent(
        Auth::Token(access_token),
        url,
        concat!("warren/", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|e| AppError::BadRequest(format!("forgejo client: {e}")))
}

fn deps_have_open(deps: &[Issue]) -> bool {
    deps.iter()
        .any(|d| matches!(d.state, Some(StateType::Open)))
}

fn issue_to_item(config_id: Uuid, host: &str, owner: &str, repo: &str, issue: Issue) -> ActionItem {
    ActionItem {
        config_id,
        host: host.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number: issue.number.unwrap_or(0),
        title: issue.title.unwrap_or_default(),
        html_url: issue
            .html_url
            .as_ref()
            .map(|u| u.as_str().to_string())
            .unwrap_or_default(),
        updated_at: issue.updated_at.and_then(|t| {
            let nanos = t.nanosecond();
            Utc.timestamp_opt(t.unix_timestamp(), nanos).single()
        }),
    }
}

/// Fetch one (host, owner, repo) triple's open items assigned to the
/// given forgejo user, skipping any whose dependency list still has an
/// open blocker. Returns `(issues, prs)`. Per-config errors propagate —
/// callers iterate configs and decide whether to swallow them (the
/// public surface below does).
pub async fn fetch_unblocked_assigned(
    config_id: Uuid,
    host: &str,
    base_url: &str,
    owner: &str,
    repo: &str,
    access_token: &str,
    forgejo_username: &str,
) -> AppResult<(Vec<ActionItem>, Vec<ActionItem>)> {
    let api = client(base_url, access_token)?;
    let issues = api
        .issue_list_issues(
            owner,
            repo,
            IssueListIssuesQuery {
                state: Some(IssueListIssuesQueryState::Open),
                assigned_by: Some(forgejo_username.to_string()),
                ..Default::default()
            },
        )
        .all()
        .await?;

    let mut issue_items = Vec::new();
    let mut pr_items = Vec::new();
    for issue in issues {
        let number = match issue.number {
            Some(n) => n,
            None => continue,
        };
        let deps = api
            .issue_list_issue_dependencies(owner, repo, number)
            .send()
            .await
            .unwrap_or_default();
        if deps_have_open(&deps) {
            continue;
        }
        let is_pr = issue.pull_request.is_some();
        let item = issue_to_item(config_id, host, owner, repo, issue);
        if is_pr {
            pr_items.push(item);
        } else {
            issue_items.push(item);
        }
    }
    Ok((issue_items, pr_items))
}

/// Per-config fetch with the same dependency-skip semantics as
/// `fetch_unblocked_assigned`. Used by the shared aggregation helpers
/// below and intentionally free-standing so its caller can pick
/// what to do with an error (log it vs. fail the response).
async fn fetch_unblocked_assigned_for_config(
    config: &crate::entity::agent_forgejo_config::Model,
) -> AppResult<(Vec<ActionItem>, Vec<ActionItem>)> {
    fetch_unblocked_assigned(
        config.id,
        &config.base_url,
        &config.base_url,
        &config.owner,
        &config.repo,
        &config.access_token,
        &config.forgejo_username,
    )
    .await
}

/// Fetch unblocked-assigned items across every forgejo config row
/// belonging to a specific agent. Used by the per-agent
/// `/api/agents/:id/action-items` endpoint. Per-config failures are
/// logged and the config's items are dropped (consistent with the
/// agents-page dashboard behavior — one broken repo must not blank
/// the rest).
pub async fn unblocked_assigned_for_agent(
    db: &Db,
    agent_id: Uuid,
) -> AppResult<(Vec<ActionItem>, Vec<ActionItem>)> {
    let configs = crate::db_ops::list_forgejo_configs_for_agent(db, agent_id).await?;
    let mut issues = Vec::new();
    let mut pull_requests = Vec::new();
    for cfg in &configs {
        if cfg.forgejo_username.trim().is_empty() {
            continue;
        }
        match fetch_unblocked_assigned_for_config(cfg).await {
            Ok((mut iss, mut prs)) => {
                issues.append(&mut iss);
                pull_requests.append(&mut prs);
            }
            Err(e) => {
                log::error!(
                    "forgejo: cfg {} ({}@{}/{}) fetch failed: {e}",
                    cfg.id,
                    cfg.forgejo_username,
                    cfg.owner,
                    cfg.repo
                );
                log::debug!("forgejo: cfg {} detail: {e:?}", cfg.id);
            }
        }
    }
    issues.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    pull_requests.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    Ok((issues, pull_requests))
}

/// Count-only variant of `unblocked_assigned_for_agent`. The
/// scheduler's pre-fire gate needs to know whether the target agent
/// has *any* unblocked forgejo item — list materialization would be
/// wasteful. Per-config errors are similarly swallowed (counted as
/// zero) so a transient forgejo outage doesn't continuously skip the
/// schedule.
pub async fn count_unblocked_assigned_for_agent(db: &Db, agent_id: Uuid) -> AppResult<(u64, u64)> {
    let configs = crate::db_ops::list_forgejo_configs_for_agent(db, agent_id).await?;
    let mut issues = 0u64;
    let mut prs = 0u64;
    for cfg in &configs {
        if cfg.forgejo_username.trim().is_empty() {
            continue;
        }
        match fetch_unblocked_assigned_for_config(cfg).await {
            Ok((iss, pr)) => {
                issues += iss.len() as u64;
                prs += pr.len() as u64;
            }
            Err(e) => {
                log::error!(
                    "forgejo: cfg {} ({}@{}/{}) count failed: {e}",
                    cfg.id,
                    cfg.forgejo_username,
                    cfg.owner,
                    cfg.repo
                );
                log::debug!("forgejo: cfg {} detail: {e:?}", cfg.id);
            }
        }
    }
    Ok((issues, prs))
}

#[allow(dead_code)]
fn _ensure_date_time_type_used(_: DateTime<Utc>) {}
