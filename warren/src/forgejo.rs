use chrono::{DateTime, TimeZone, Utc};
use forgejo_api::structs::{Issue, IssueListIssuesQuery, IssueListIssuesQueryState, StateType};
use forgejo_api::{Auth, Forgejo};
use url::Url;
use uuid::Uuid;

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

fn issue_to_item(owner: &str, repo: &str, issue: Issue) -> ActionItem {
    ActionItem {
        config_id: Uuid::nil(),
        host: String::new(),
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

pub async fn fetch_unblocked_assigned(
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
        let item = issue_to_item(owner, repo, issue);
        if is_pr {
            pr_items.push(item);
        } else {
            issue_items.push(item);
        }
    }
    Ok((issue_items, pr_items))
}

#[allow(dead_code)]
fn _ensure_date_time_type_used(_: DateTime<Utc>) {}
