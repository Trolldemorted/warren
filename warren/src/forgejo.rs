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

/// Build the exact URL forgejo-api will GET for `issue_list_issues`.
/// Mirrors the path layout in `forgejo-api`'s generated endpoint
/// (`/api/v1/repos/{owner}/{repo}/issues`) and percent-encodes owner
/// / repo the same way `urlencoding::encode` does. The query is
/// passed verbatim so callers can attach their own params without
/// double-encoding — pass `""` for no query string.
fn issue_list_url(base_url: &str, owner: &str, repo: &str, query: &str) -> AppResult<String> {
    let mut url = Url::parse(base_url)
        .map_err(|e| AppError::BadRequest(format!("invalid forgejo base_url: {e}")))?;
    {
        let mut segs = url.path_segments_mut().map_err(|_| {
            AppError::BadRequest(format!("forgejo base_url has no path: {base_url}"))
        })?;
        segs.pop_if_empty();
        segs.push("api");
        segs.push("v1");
        segs.push("repos");
        segs.push(owner);
        segs.push(repo);
        segs.push("issues");
    }
    if !query.is_empty() {
        url.set_query(Some(query));
    }
    Ok(url.to_string())
}

/// Build the exact URL forgejo-api will GET for
/// `issue_list_issue_dependencies`. Same path layout as
/// `issue_list_url`, plus `/issues/{index}/dependencies`.
fn issue_deps_url(base_url: &str, owner: &str, repo: &str, index: i64) -> AppResult<String> {
    let mut url = Url::parse(base_url)
        .map_err(|e| AppError::BadRequest(format!("invalid forgejo base_url: {e}")))?;
    {
        let mut segs = url.path_segments_mut().map_err(|_| {
            AppError::BadRequest(format!("forgejo base_url has no path: {base_url}"))
        })?;
        segs.pop_if_empty();
        segs.push("api");
        segs.push("v1");
        segs.push("repos");
        segs.push(owner);
        segs.push(repo);
        segs.push("issues");
        segs.push(&index.to_string());
        segs.push("dependencies");
    }
    Ok(url.to_string())
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
    let issues_url = issue_list_url(
        base_url,
        owner,
        repo,
        &format!("state=open&assigned_by={forgejo_username}"),
    )?;
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
        .await
        .map_err(|e| AppError::BadGateway(format!("GET {issues_url}: {e}")))?;

    let mut issue_items = Vec::new();
    let mut pr_items = Vec::new();
    for issue in issues {
        let number = match issue.number {
            Some(n) => n,
            None => continue,
        };
        let deps_url = issue_deps_url(base_url, owner, repo, number)?;
        let deps = match api
            .issue_list_issue_dependencies(owner, repo, number)
            .send()
            .await
            .map_err(|e| AppError::BadGateway(format!("GET {deps_url}: {e}")))
        {
            Ok(d) => d,
            Err(e) => {
                log::warn!(
                    "forgejo: cfg {} GET {} failed ({e}); treating as unblocked",
                    config_id,
                    deps_url
                );
                Vec::new()
            }
        };
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

/// Fetch one (host, owner, repo) triple's open, *unassigned* items
/// that carry any of the given labels (OR semantics), skipping any
/// whose dependency list still has an open blocker. Empty `labels`
/// short-circuits with `(vec![], vec![])` — sending `labels=""` to
/// forgejo is undefined and would otherwise pull the whole open
/// issue list at scale.
///
/// The forgejo-api `IssueListIssuesQuery` has no `assignee` field,
/// so the "unassigned" filter has to happen client-side. An item is
/// considered assigned if `issue.assignee.is_some()` or
/// `issue.assignees` is non-empty. Same dependency-skip semantics as
/// `fetch_unblocked_assigned`.
pub async fn fetch_unblocked_unassigned_with_label(
    config_id: Uuid,
    host: &str,
    base_url: &str,
    owner: &str,
    repo: &str,
    access_token: &str,
    labels: &[String],
) -> AppResult<(Vec<ActionItem>, Vec<ActionItem>)> {
    if labels.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let api = client(base_url, access_token)?;
    let labels_csv = labels.join(",");
    let issues_url = issue_list_url(
        base_url,
        owner,
        repo,
        &format!("state=open&labels={labels_csv}"),
    )?;
    let issues = api
        .issue_list_issues(
            owner,
            repo,
            IssueListIssuesQuery {
                state: Some(IssueListIssuesQueryState::Open),
                labels: Some(labels_csv),
                ..Default::default()
            },
        )
        .all()
        .await
        .map_err(|e| AppError::BadGateway(format!("GET {issues_url}: {e}")))?;

    let mut issue_items = Vec::new();
    let mut pr_items = Vec::new();
    for issue in issues {
        if issue.assignee.is_some()
            || issue
                .assignees
                .as_ref()
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        {
            continue;
        }
        let number = match issue.number {
            Some(n) => n,
            None => continue,
        };
        let deps_url = issue_deps_url(base_url, owner, repo, number)?;
        let deps = match api
            .issue_list_issue_dependencies(owner, repo, number)
            .send()
            .await
            .map_err(|e| AppError::BadGateway(format!("GET {deps_url}: {e}")))
        {
            Ok(d) => d,
            Err(e) => {
                log::warn!(
                    "forgejo: cfg {} GET {} failed ({e}); treating as unblocked",
                    config_id,
                    deps_url
                );
                Vec::new()
            }
        };
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

/// Per-config wrapper around `fetch_unblocked_unassigned_with_label`.
async fn fetch_unblocked_unassigned_for_config(
    config: &crate::entity::agent_forgejo_config::Model,
    labels: &[String],
) -> AppResult<(Vec<ActionItem>, Vec<ActionItem>)> {
    fetch_unblocked_unassigned_with_label(
        config.id,
        &config.base_url,
        &config.base_url,
        &config.owner,
        &config.repo,
        &config.access_token,
        labels,
    )
    .await
}

/// Fetch unblocked work items (assigned + unassigned-with-label)
/// across every forgejo config row belonging to a specific agent.
/// Used by the per-agent `/api/agents/:id/action-items` endpoint.
/// Pass `&[]` for the labels slice to keep assigned-only semantics —
/// the JSON contract currently has no place to surface schedule
/// labels, so callers without a schedule context pass `&[]`.
/// Per-config failures are logged and the config's items are dropped
/// (consistent with the agents-page dashboard behavior — one broken
/// repo must not blank the rest).
pub async fn unblocked_work_items_for_agent(
    db: &Db,
    agent_id: Uuid,
    additional_labels: &[String],
) -> AppResult<((Vec<ActionItem>, Vec<ActionItem>), Vec<String>)> {
    let configs = crate::db_ops::list_forgejo_configs_for_agent(db, agent_id).await?;
    let mut issues = Vec::new();
    let mut pull_requests = Vec::new();
    let mut errors = Vec::new();
    for cfg in &configs {
        if cfg.forgejo_username.trim().is_empty() {
            log::warn!(
                "forgejo: cfg {} ({}@{}/{}) skipped: forgejo_username is empty",
                cfg.id,
                cfg.base_url,
                cfg.owner,
                cfg.repo
            );
            continue;
        }
        match fetch_unblocked_assigned_for_config(cfg).await {
            Ok((mut iss, mut prs)) => {
                issues.append(&mut iss);
                pull_requests.append(&mut prs);
            }
            Err(e) => {
                log::error!(
                    "forgejo: cfg {} ({}@{}/{}) assigned fetch failed: {e}",
                    cfg.id,
                    cfg.forgejo_username,
                    cfg.owner,
                    cfg.repo
                );
                log::debug!("forgejo: cfg {} detail: {e:?}", cfg.id);
                errors.push(format!(
                    "{}/{}: {}",
                    cfg.owner,
                    cfg.repo,
                    truncate(&e.to_string(), PER_CONFIG_ERROR_MAX)
                ));
            }
        }
        if !additional_labels.is_empty() {
            match fetch_unblocked_unassigned_for_config(cfg, additional_labels).await {
                Ok((mut iss, mut prs)) => {
                    issues.append(&mut iss);
                    pull_requests.append(&mut prs);
                }
                Err(e) => {
                    log::error!(
                        "forgejo: cfg {} ({}@{}/{}) unassigned-by-label fetch failed: {e}",
                        cfg.id,
                        cfg.forgejo_username,
                        cfg.owner,
                        cfg.repo
                    );
                    log::debug!("forgejo: cfg {} detail: {e:?}", cfg.id);
                    errors.push(format!(
                        "{}/{}: {}",
                        cfg.owner,
                        cfg.repo,
                        truncate(&e.to_string(), PER_CONFIG_ERROR_MAX)
                    ));
                }
            }
        }
    }
    issues.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    pull_requests.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
    Ok(((issues, pull_requests), errors))
}

/// Count-only variant of `unblocked_work_items_for_agent`. The
/// scheduler's pre-fire gate needs to know whether the target agent
/// has *any* unblocked forgejo item — list materialization would be
/// wasteful. Per-config errors are similarly swallowed (counted as
/// zero) so a transient forgejo outage doesn't continuously skip the
/// schedule.
pub async fn count_work_items_for_agent(
    db: &Db,
    agent_id: Uuid,
    additional_labels: &[String],
) -> AppResult<((u64, u64), Vec<String>)> {
    let configs = crate::db_ops::list_forgejo_configs_for_agent(db, agent_id).await?;
    let mut issues = 0u64;
    let mut prs = 0u64;
    let mut errors = Vec::new();
    for cfg in &configs {
        if cfg.forgejo_username.trim().is_empty() {
            log::warn!(
                "forgejo: cfg {} ({}@{}/{}) skipped: forgejo_username is empty",
                cfg.id,
                cfg.base_url,
                cfg.owner,
                cfg.repo
            );
            continue;
        }
        match fetch_unblocked_assigned_for_config(cfg).await {
            Ok((iss, pr)) => {
                issues += iss.len() as u64;
                prs += pr.len() as u64;
            }
            Err(e) => {
                log::error!(
                    "forgejo: cfg {} ({}@{}/{}) assigned count failed: {e}",
                    cfg.id,
                    cfg.forgejo_username,
                    cfg.owner,
                    cfg.repo
                );
                log::debug!("forgejo: cfg {} detail: {e:?}", cfg.id);
                errors.push(format!(
                    "{}/{}: {}",
                    cfg.owner,
                    cfg.repo,
                    truncate(&e.to_string(), PER_CONFIG_ERROR_MAX)
                ));
            }
        }
        if !additional_labels.is_empty() {
            match fetch_unblocked_unassigned_for_config(cfg, additional_labels).await {
                Ok((iss, pr)) => {
                    issues += iss.len() as u64;
                    prs += pr.len() as u64;
                }
                Err(e) => {
                    log::error!(
                        "forgejo: cfg {} ({}@{}/{}) unassigned-by-label count failed: {e}",
                        cfg.id,
                        cfg.forgejo_username,
                        cfg.owner,
                        cfg.repo
                    );
                    log::debug!("forgejo: cfg {} detail: {e:?}", cfg.id);
                    errors.push(format!(
                        "{}/{}: {}",
                        cfg.owner,
                        cfg.repo,
                        truncate(&e.to_string(), PER_CONFIG_ERROR_MAX)
                    ));
                }
            }
        }
    }
    Ok(((issues, prs), errors))
}

#[allow(dead_code)]
fn _ensure_date_time_type_used(_: DateTime<Utc>) {}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 1);
    out.push_str(&s[..end]);
    out.push('…');
    out
}

/// Cap for the per-config error string that surfaces in the
/// agents-page dashboard. Sized so the full forgejo URL (`GET
/// {base}/api/v1/repos/{owner}/{repo}/issues?…`) plus a short inner
/// error survives in the badge `title` attribute.
const PER_CONFIG_ERROR_MAX: usize = 240;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_list_url_strips_trailing_slash_and_appends_path() {
        let url = issue_list_url(
            "https://git.stronk.pw/",
            "Patrician3",
            "Patrizia3",
            "state=open&assigned_by=p3-claude-rewrite-miniax",
        )
        .expect("URL must build");
        assert_eq!(
            url,
            "https://git.stronk.pw/api/v1/repos/Patrician3/Patrizia3/issues?state=open&assigned_by=p3-claude-rewrite-miniax"
        );
    }

    #[test]
    fn issue_list_url_percent_encodes_owner_and_repo() {
        let url = issue_list_url("https://forge.example/", "foo bar", "baz/qux", "")
            .expect("URL must build");
        assert_eq!(
            url,
            "https://forge.example/api/v1/repos/foo%20bar/baz%2Fqux/issues"
        );
    }

    #[test]
    fn issue_deps_url_appends_issue_index_and_segment() {
        let url =
            issue_deps_url("https://git.example", "owner", "repo", 116).expect("URL must build");
        assert_eq!(
            url,
            "https://git.example/api/v1/repos/owner/repo/issues/116/dependencies"
        );
    }
}
