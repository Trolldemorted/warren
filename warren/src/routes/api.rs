use crate::auth::{AgentAuth, AuthContext, SESSION_COOKIE};
use crate::db::Db;
use crate::entity::{
    agent, agent_forgejo_config, channel, request, scheduled_prompt, scheduled_prompt_run,
};
use crate::error::{AppError, AppResult};
use crate::ids::{new_agent_token, new_session_token};
use crate::models::{
    ActionItemsResponse, AgentForgejoConfigNew, AgentForgejoConfigPatch, AgentNew, AgentPatch,
    ChannelNew, ChannelPatch, LoginReq, LoginRes, RequestNew, RequestRespond, ScheduledPromptNew,
    ScheduledPromptPatch,
};
use crate::{auth, AppState};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/agents", get(api_list_agents).post(api_create_agent))
        .route("/api/agents/me", get(api_me))
        .route(
            "/api/agents/:id",
            get(api_get_agent)
                .put(api_update_agent)
                .delete(api_delete_agent),
        )
        // The `/api/agents/:id/claude/...` JSON routes are merged in
        // `main.rs` via `state.live.router()`. They live in
        // `rabbit-lib::server::http` and use the lib's auth gate
        // directly, so they don't belong on the `AppState`-typed API
        // router anymore.
        .route(
            "/api/requests",
            get(api_list_requests).post(api_create_request),
        )
        .route("/api/inbox", get(api_inbox))
        .route("/api/requests/mine", get(api_requests_mine))
        .route(
            "/api/requests/:id",
            get(api_get_request).delete(api_delete_request),
        )
        .route("/api/requests/:id/claim", post(api_claim_request))
        .route("/api/requests/:id/respond", post(api_respond_request))
        .route("/api/requests/:id/approve", post(api_approve_request))
        .route("/api/requests/:id/reject", post(api_reject_request))
        .route(
            "/api/requests/:id/accept-response",
            post(api_accept_response),
        )
        .route(
            "/api/requests/:id/reject-response",
            post(api_reject_response),
        )
        .route(
            "/api/requests/:id/acknowledge",
            post(api_acknowledge_request),
        )
        .route(
            "/api/requests/:id/unacknowledge",
            post(api_unacknowledge_request),
        )
        .route("/api/requests/:id/status", post(api_set_request_status))
        .route(
            "/api/channels",
            get(api_list_channels).post(api_create_channel),
        )
        .route(
            "/api/channels/:id",
            get(api_get_channel)
                .put(api_update_channel)
                .delete(api_delete_channel),
        )
        .route(
            "/api/scheduled-prompts",
            get(api_list_scheduled_prompts).post(api_create_scheduled_prompt),
        )
        .route(
            "/api/scheduled-prompts/:id",
            get(api_get_scheduled_prompt)
                .put(api_update_scheduled_prompt)
                .delete(api_delete_scheduled_prompt),
        )
        .route(
            "/api/scheduled-prompts/:id/run-now",
            post(api_run_scheduled_prompt_now),
        )
        .route(
            "/api/agents/:id/forgejo-configs",
            get(api_list_agent_forgejo_configs).post(api_create_agent_forgejo_config),
        )
        .route("/api/agents/:id/action-items", get(api_agent_action_items))
        .route(
            "/api/forgejo-configs/:config_id",
            get(api_get_forgejo_config)
                .put(api_update_forgejo_config)
                .delete(api_delete_forgejo_config),
        )
}

async fn build_request_not_found(db: &Db, agent: &AgentAuth, requested: Uuid) -> AppError {
    let rows = crate::db_ops::list_inbox_for_agent(
        db,
        agent.0.id,
        &agent.0.class,
        agent.0.kind.as_deref(),
    )
    .await
    .unwrap_or_default();
    let ids: Vec<Uuid> = rows.into_iter().map(|r| r.id).collect();
    AppError::RequestNotFound(requested, ids)
}

async fn run_or_classify_missing<F>(
    db: &Db,
    agent: &AgentAuth,
    requested: Uuid,
    fut: F,
) -> AppResult<request::Model>
where
    F: std::future::Future<Output = AppResult<request::Model>>,
{
    match fut.await {
        Ok(r) => Ok(r),
        Err(AppError::Conflict(msg)) => match crate::db_ops::get_request(db, requested).await? {
            None => Err(build_request_not_found(db, agent, requested).await),
            Some(_) => Err(AppError::Conflict(msg)),
        },
        Err(e) => Err(e),
    }
}

async fn api_login(
    State(state): State<AppState>,
    Json(req): Json<LoginReq>,
) -> AppResult<impl IntoResponse> {
    if !crate::auth::psk_matches(&req.password, &state.config.admin_psk) {
        return Err(AppError::Unauthorized);
    }
    let token = new_session_token();
    auth::create_admin_session(&state.db, &token, state.config.session_ttl_hours).await?;
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        state.config.session_ttl_hours * 3600
    );
    let value =
        HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    Ok(([(header::SET_COOKIE, value)], Json(LoginRes { ok: true })))
}

async fn api_logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> AppResult<StatusCode> {
    if let Some(token) = crate::auth::read_session_cookie(&headers) {
        crate::db_ops::delete_admin_session(&state.db, &token).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn api_list_agents(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<agent::Model>>> {
    ctx.require_admin()?;
    let agents = crate::db_ops::list_agents(&state.db).await?;
    Ok(Json(agents))
}

async fn api_get_agent(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<agent::Model>> {
    ctx.require_admin()?;
    let agent = crate::db_ops::get_agent(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(agent))
}

async fn api_me(State(state): State<AppState>, ctx: AuthContext) -> AppResult<Json<MeResponse>> {
    let auth = ctx.require_agent()?;
    let available_channels =
        crate::db_ops::channels_for_sender(&state.db, &auth.0.class, auth.0.kind.as_deref())
            .await?;
    Ok(Json(MeResponse {
        agent: auth.0.clone(),
        available_channels,
    }))
}

async fn api_create_agent(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(new): Json<AgentNew>,
) -> AppResult<Json<agent::Model>> {
    ctx.require_admin()?;
    validate_agent_new(&new)?;
    let _ = new_agent_token();
    let agent = crate::db_ops::create_agent(&state.db, &new).await?;
    Ok(Json(agent))
}

async fn api_update_agent(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(patch): Json<AgentPatch>,
) -> AppResult<Json<agent::Model>> {
    ctx.require_admin()?;
    let agent = crate::db_ops::patch_agent(&state.db, id, &patch).await?;
    Ok(Json(agent))
}

async fn api_delete_agent(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::delete_agent(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_agent_new(n: &AgentNew) -> AppResult<()> {
    if n.name.trim().is_empty() {
        return Err(AppError::BadRequest("name required".into()));
    }
    if n.class.trim().is_empty() {
        return Err(AppError::BadRequest("class required".into()));
    }
    if n.model.trim().is_empty() {
        return Err(AppError::BadRequest("model required".into()));
    }
    Ok(())
}

#[derive(Deserialize, Default)]
struct ListQuery {
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn api_list_requests(
    State(state): State<AppState>,
    ctx: AuthContext,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<request::Model>>> {
    ctx.require_admin()?;
    let limit = q.limit.unwrap_or(50).clamp(1, 500) as u64;
    let offset = q.offset.unwrap_or(0).max(0) as u64;
    let status = parse_request_status(q.status.as_deref())?;
    let rows = crate::db_ops::list_all_requests(&state.db, status, limit, offset).await?;
    Ok(Json(rows))
}

async fn api_inbox(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<request::Model>>> {
    let a = ctx.require_agent()?;
    let rows =
        crate::db_ops::list_inbox_for_agent(&state.db, a.0.id, &a.0.class, a.0.kind.as_deref())
            .await?;
    Ok(Json(rows))
}

async fn api_requests_mine(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<request::Model>>> {
    let a = ctx.require_agent()?;
    let rows =
        crate::db_ops::list_history_for_agent(&state.db, a.0.id, &a.0.class, a.0.kind.as_deref())
            .await?;
    Ok(Json(rows))
}

async fn api_create_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(new): Json<RequestNew>,
) -> AppResult<Json<request::Model>> {
    // Admin POSTs auto-skip request approval; agent POSTs go through review
    // unless the channel disables request approval.
    let (initial_status, sender_agent_id) = match &ctx {
        AuthContext::Admin(_) => (request::AWAITING_AGENT_REQUEST_CLAIM, None),
        AuthContext::Agent(a) => (request::AWAITING_ADMIN_REQUEST_APPROVAL, Some(a.0.id)),
    };
    if new.target_class.trim().is_empty() {
        return Err(AppError::BadRequest("target_class required".into()));
    }
    validate_request_new(&new)?;
    if let AuthContext::Agent(a) = &ctx {
        let channel_id = new
            .channel_id
            .ok_or_else(|| AppError::BadRequest("channel_id required".into()))?;
        crate::db_ops::channel_authorizes(
            &state.db,
            channel_id,
            &a.0.class,
            a.0.kind.as_deref(),
            &new.target_class,
            new.target_type.as_deref(),
        )
        .await?;
    }
    let initial_status = if matches!(&ctx, AuthContext::Agent(_)) {
        if let Some(channel_id) = new.channel_id {
            if !crate::db_ops::channel_requires_request_approval(&state.db, channel_id).await? {
                request::AWAITING_AGENT_REQUEST_CLAIM
            } else {
                initial_status
            }
        } else {
            initial_status
        }
    } else {
        initial_status
    };
    let r = crate::db_ops::create_request(&state.db, &new, initial_status, sender_agent_id).await?;
    Ok(Json(r))
}

async fn api_get_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<request::Model>> {
    match crate::db_ops::get_request(&state.db, id).await? {
        None => match &ctx {
            AuthContext::Admin(_) => Err(AppError::NotFound),
            AuthContext::Agent(a) => Err(build_request_not_found(&state.db, a, id).await),
        },
        Some(r) => match &ctx {
            AuthContext::Admin(_) => Ok(Json(r)),
            AuthContext::Agent(a) => {
                let sent_self = r.sender_agent_id.map(|s| s == a.0.id).unwrap_or(false);
                let claims_self = r.claimed_by.map(|cb| cb == a.0.id).unwrap_or(false);
                let in_inbox = r.status == request::AWAITING_AGENT_REQUEST_CLAIM
                    && r.claimed_by.is_none()
                    && r.target_class == a.0.class
                    && (r.target_type.is_none() || r.target_type.as_deref() == a.0.kind.as_deref());
                if sent_self || claims_self || in_inbox {
                    Ok(Json(r))
                } else {
                    Err(AppError::Forbidden)
                }
            }
        },
    }
}

async fn api_delete_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::delete_request(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_claim_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<request::Model>> {
    let agent = ctx.require_agent()?;
    let r = run_or_classify_missing(&state.db, agent, id, async {
        crate::db_ops::claim_request(&state.db, id, agent.0.id).await
    })
    .await?;
    Ok(Json(r))
}

async fn api_respond_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(body): Json<RequestRespond>,
) -> AppResult<Json<request::Model>> {
    let agent = ctx.require_agent()?;
    validate_request_respond(&body)?;
    let r = run_or_classify_missing(&state.db, agent, id, async {
        crate::db_ops::respond_to_request(&state.db, id, agent.0.id, &body.response).await
    })
    .await?;
    Ok(Json(r))
}

async fn api_approve_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::set_request_status(
        &state.db,
        id,
        request::AWAITING_ADMIN_REQUEST_APPROVAL,
        request::AWAITING_AGENT_REQUEST_CLAIM,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_reject_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::set_request_status(
        &state.db,
        id,
        request::AWAITING_ADMIN_REQUEST_APPROVAL,
        request::REJECTED,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_accept_response(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::accept_request_response(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_reject_response(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::reject_request_response(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_acknowledge_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<request::Model>> {
    match &ctx {
        AuthContext::Admin(_) => {
            let r = crate::db_ops::acknowledge_request(&state.db, id, Uuid::nil(), true).await?;
            Ok(Json(r))
        }
        AuthContext::Agent(agent) => {
            let r = run_or_classify_missing(&state.db, agent, id, async {
                crate::db_ops::acknowledge_request(&state.db, id, agent.0.id, false).await
            })
            .await?;
            Ok(Json(r))
        }
    }
}

async fn api_unacknowledge_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::unacknowledge_request(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RequestSetStatus {
    status: String,
}

async fn api_set_request_status(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(body): Json<RequestSetStatus>,
) -> AppResult<Json<request::Model>> {
    ctx.require_admin()?;
    let new_status = parse_request_status(Some(&body.status))?
        .ok_or_else(|| AppError::BadRequest(format!("unknown status '{}'", body.status)))?;
    let r = crate::db_ops::set_request_status_admin(&state.db, id, new_status).await?;
    Ok(Json(r))
}

fn parse_request_status(s: Option<&str>) -> AppResult<Option<i16>> {
    match s {
        None => Ok(None),
        Some("awaiting_admin_request_approval") => {
            Ok(Some(request::AWAITING_ADMIN_REQUEST_APPROVAL))
        }
        Some("awaiting_agent_request_claim") => Ok(Some(request::AWAITING_AGENT_REQUEST_CLAIM)),
        Some("awaiting_agent_response") => Ok(Some(request::AWAITING_AGENT_RESPONSE)),
        Some("awaiting_admin_response_approval") => {
            Ok(Some(request::AWAITING_ADMIN_RESPONSE_APPROVAL))
        }
        Some("awaiting_agent_response_acknowledge") => {
            Ok(Some(request::AWAITING_AGENT_RESPONSE_ACKNOWLEDGE))
        }
        Some("done") => Ok(Some(request::DONE)),
        Some("rejected") => Ok(Some(request::REJECTED)),
        Some(other) => Err(AppError::BadRequest(format!("unknown status '{other}'"))),
    }
}

#[derive(serde::Serialize)]
struct MeResponse {
    #[serde(flatten)]
    agent: agent::Model,
    available_channels: Vec<channel::Model>,
}

async fn api_list_channels(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<channel::Model>>> {
    ctx.require_admin()?;
    Ok(Json(crate::db_ops::list_channels(&state.db).await?))
}

async fn api_get_channel(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<channel::Model>> {
    ctx.require_admin()?;
    let ch = crate::db_ops::get_channel(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(ch))
}

async fn api_create_channel(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(new): Json<ChannelNew>,
) -> AppResult<Json<channel::Model>> {
    ctx.require_admin()?;
    validate_channel_new(&new)?;
    let ch = crate::db_ops::create_channel(&state.db, &new).await?;
    Ok(Json(ch))
}

async fn api_update_channel(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(patch): Json<ChannelPatch>,
) -> AppResult<Json<channel::Model>> {
    ctx.require_admin()?;
    let ch = crate::db_ops::patch_channel(&state.db, id, &patch).await?;
    Ok(Json(ch))
}

async fn api_delete_channel(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::delete_channel(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_channel_new(n: &ChannelNew) -> AppResult<()> {
    if n.sender_class.trim().is_empty() {
        return Err(AppError::BadRequest("sender_class required".into()));
    }
    if n.receiver_class.trim().is_empty() {
        return Err(AppError::BadRequest("receiver_class required".into()));
    }
    if n.description.trim().is_empty() {
        return Err(AppError::BadRequest("description required".into()));
    }
    Ok(())
}

fn validate_scheduled_prompt_new(n: &ScheduledPromptNew) -> AppResult<()> {
    // Scope gate — must be one of the two known strings and the
    // address fields must match the scope (mutual exclusion).
    match n.scope.as_str() {
        "team" => {
            let class = n
                .target_class
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::BadRequest("target_class required for team scope".into())
                })?;
            if class.is_empty() {
                return Err(AppError::BadRequest("target_class required".into()));
            }
            if n.agent_id.is_some() {
                return Err(AppError::BadRequest(
                    "agent_id must be null for team scope".into(),
                ));
            }
        }
        "agent" => {
            if n.agent_id.is_none() {
                return Err(AppError::BadRequest(
                    "agent_id required for agent scope".into(),
                ));
            }
            if n.target_class.is_some() || n.target_kind.is_some() {
                return Err(AppError::BadRequest(
                    "target_class/target_kind must be null for agent scope".into(),
                ));
            }
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown scope '{other}' (expected 'team' or 'agent')"
            )));
        }
    }
    if n.name.trim().is_empty() {
        return Err(AppError::BadRequest("name required".into()));
    }
    if n.prompt_text.trim().is_empty() {
        return Err(AppError::BadRequest("prompt_text required".into()));
    }
    if n.interval_seconds < 1 {
        return Err(AppError::BadRequest("interval_seconds must be >= 1".into()));
    }
    if !(0..=100).contains(&n.weekly_safety_buffer_pct) {
        return Err(AppError::BadRequest(
            "weekly_safety_buffer_pct must be 0..=100".into(),
        ));
    }
    if !(0..=100).contains(&n.session_safety_buffer_pct) {
        return Err(AppError::BadRequest(
            "session_safety_buffer_pct must be 0..=100".into(),
        ));
    }
    if let Some(c) = n.context_clear_threshold_pct {
        if !(0..=100).contains(&c) {
            return Err(AppError::BadRequest(
                "context_clear_threshold_pct must be 0..=100".into(),
            ));
        }
    }
    Ok(())
}

fn validate_scheduled_prompt_patch(p: &ScheduledPromptPatch) -> AppResult<()> {
    // Scope is intentionally not mutable through the patch path —
    // retargeting between team/agent would orphan the run-history
    // rows bound to the previous address. The UI never sends it.
    if let Some(s) = &p.scope {
        if s != "team" && s != "agent" {
            return Err(AppError::BadRequest(format!(
                "unknown scope '{s}' (expected 'team' or 'agent')"
            )));
        }
    }
    if let Some(n) = &p.name {
        if n.trim().is_empty() {
            return Err(AppError::BadRequest("name required".into()));
        }
    }
    if let Some(t) = &p.prompt_text {
        if t.trim().is_empty() {
            return Err(AppError::BadRequest("prompt_text required".into()));
        }
    }
    if let Some(i) = p.interval_seconds {
        if i < 1 {
            return Err(AppError::BadRequest("interval_seconds must be >= 1".into()));
        }
    }
    if let Some(w) = p.weekly_safety_buffer_pct {
        if !(0..=100).contains(&w) {
            return Err(AppError::BadRequest(
                "weekly_safety_buffer_pct must be 0..=100".into(),
            ));
        }
    }
    if let Some(s) = p.session_safety_buffer_pct {
        if !(0..=100).contains(&s) {
            return Err(AppError::BadRequest(
                "session_safety_buffer_pct must be 0..=100".into(),
            ));
        }
    }
    if let Some(c) = p.context_clear_threshold_pct {
        if !(0..=100).contains(&c) {
            return Err(AppError::BadRequest(
                "context_clear_threshold_pct must be 0..=100".into(),
            ));
        }
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct ScheduledPromptWithRuns {
    #[serde(flatten)]
    prompt: scheduled_prompt::Model,
    recent_runs: Vec<scheduled_prompt_run::Model>,
}

async fn api_list_scheduled_prompts(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<scheduled_prompt::Model>>> {
    ctx.require_admin()?;
    let prompts = crate::db_ops::list_scheduled_prompts(&state.db).await?;
    Ok(Json(prompts))
}

async fn api_get_scheduled_prompt(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<ScheduledPromptWithRuns>> {
    ctx.require_admin()?;
    let prompt = crate::db_ops::get_scheduled_prompt(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let runs = crate::db_ops::list_runs_for_scheduled_prompt(&state.db, id, 50).await?;
    Ok(Json(ScheduledPromptWithRuns {
        prompt,
        recent_runs: runs,
    }))
}

async fn api_create_scheduled_prompt(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(mut new): Json<ScheduledPromptNew>,
) -> AppResult<Json<scheduled_prompt::Model>> {
    ctx.require_admin()?;
    // Trim incoming string fields; empty strings become None so the
    // validators can reason about Option-only semantics.
    if let Some(c) = &new.target_class {
        let t = c.trim();
        new.target_class = if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        };
    }
    if let Some(k) = &new.target_kind {
        let t = k.trim();
        new.target_kind = if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        };
    }
    validate_scheduled_prompt_new(&new)?;
    let prompt = crate::db_ops::create_scheduled_prompt(&state.db, &new).await?;
    Ok(Json(prompt))
}

async fn api_update_scheduled_prompt(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(patch): Json<ScheduledPromptPatch>,
) -> AppResult<Json<scheduled_prompt::Model>> {
    ctx.require_admin()?;
    validate_scheduled_prompt_patch(&patch)?;
    let prompt = crate::db_ops::update_scheduled_prompt(&state.db, id, &patch).await?;
    Ok(Json(prompt))
}

async fn api_delete_scheduled_prompt(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::delete_scheduled_prompt(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_run_scheduled_prompt_now(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<scheduled_prompt_run::Model>> {
    ctx.require_admin()?;
    let prompt = crate::db_ops::get_scheduled_prompt(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !prompt.enabled {
        return Err(AppError::Conflict("schedule is disabled".into()));
    }
    let arc = std::sync::Arc::new(state.clone());
    if let Err(e) = crate::scheduler::fire_prompt(arc, prompt).await {
        return Err(AppError::Internal(e));
    }
    let runs = crate::db_ops::list_runs_for_scheduled_prompt(&state.db, id, 1).await?;
    let run = runs.into_iter().next().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "fire_prompt completed without inserting a run row"
        ))
    })?;
    Ok(Json(run))
}

/// §Reject empty payloads: a `RequestNew` whose `payload` is empty (or
/// whitespace-only) is rejected at the API boundary. Empty payloads
/// produce meaningless inbox rows (the receiving agent has no
/// instructions to act on) and historically surfaced as confusing
/// "agent claimed an empty request" log entries. Returns
/// `AppError::BadRequest("payload required")` so the CLI can surface a
/// helpful message instead of a 500.
fn validate_request_new(n: &RequestNew) -> AppResult<()> {
    if n.payload.trim().is_empty() {
        return Err(AppError::BadRequest("payload required".into()));
    }
    Ok(())
}

/// §Reject empty payloads: same rationale as `validate_request_new`, but
/// for agent responses to claimed requests. An empty response leaves the
/// request stuck in `AWAITING_AGENT_RESPONSE` forever — the original
/// claimer has nothing to ship back to the admin reviewer, and the
/// request effectively times out in the inbox.
fn validate_request_respond(r: &RequestRespond) -> AppResult<()> {
    if r.response.trim().is_empty() {
        return Err(AppError::BadRequest("response required".into()));
    }
    Ok(())
}

// --- forgejo configs --------------------------------------------------------

fn validate_forgejo_config_new(n: &AgentForgejoConfigNew) -> AppResult<()> {
    let require = |name: &str, val: &str| -> AppResult<()> {
        if val.trim().is_empty() {
            return Err(AppError::BadRequest(format!("{name} required")));
        }
        Ok(())
    };
    require("forgejo_username", &n.forgejo_username)?;
    require("base_url", &n.base_url)?;
    require("owner", &n.owner)?;
    require("repo", &n.repo)?;
    require("access_token", &n.access_token)?;
    let parsed = url::Url::parse(n.base_url.trim())
        .map_err(|e| AppError::BadRequest(format!("invalid base_url: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::BadRequest(
            "base_url must be http or https".into(),
        ));
    }
    Ok(())
}

async fn api_list_agent_forgejo_configs(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(agent_id): Path<Uuid>,
) -> AppResult<Json<Vec<agent_forgejo_config::Model>>> {
    ctx.require_admin()?;
    let _ = crate::db_ops::get_agent(&state.db, agent_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = crate::db_ops::list_forgejo_configs_for_agent(&state.db, agent_id).await?;
    Ok(Json(rows))
}

async fn api_create_agent_forgejo_config(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(agent_id): Path<Uuid>,
    Json(mut new): Json<AgentForgejoConfigNew>,
) -> AppResult<Json<agent_forgejo_config::Model>> {
    ctx.require_admin()?;
    let _ = crate::db_ops::get_agent(&state.db, agent_id)
        .await?
        .ok_or(AppError::NotFound)?;
    new.forgejo_username = new.forgejo_username.trim().to_string();
    new.base_url = new.base_url.trim().to_string();
    new.owner = new.owner.trim().to_string();
    new.repo = new.repo.trim().to_string();
    validate_forgejo_config_new(&new)?;
    let row = crate::db_ops::create_forgejo_config(&state.db, agent_id, &new).await?;
    Ok(Json(row))
}

async fn api_get_forgejo_config(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(config_id): Path<Uuid>,
) -> AppResult<Json<agent_forgejo_config::Model>> {
    ctx.require_admin()?;
    let row = crate::db_ops::get_forgejo_config(&state.db, config_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(row))
}

async fn api_update_forgejo_config(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(config_id): Path<Uuid>,
    Json(patch): Json<AgentForgejoConfigPatch>,
) -> AppResult<Json<agent_forgejo_config::Model>> {
    ctx.require_admin()?;
    if let Some(u) = &patch.base_url {
        let parsed = url::Url::parse(u.trim())
            .map_err(|e| AppError::BadRequest(format!("invalid base_url: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AppError::BadRequest(
                "base_url must be http or https".into(),
            ));
        }
    }
    let row = crate::db_ops::update_forgejo_config(&state.db, config_id, &patch).await?;
    Ok(Json(row))
}

async fn api_delete_forgejo_config(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(config_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::delete_forgejo_config(&state.db, config_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_agent_action_items(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(agent_id): Path<Uuid>,
) -> AppResult<Json<ActionItemsResponse>> {
    ctx.require_admin()?;
    let _ = crate::db_ops::get_agent(&state.db, agent_id)
        .await?
        .ok_or(AppError::NotFound)?;

    let (issues, pull_requests) =
        crate::forgejo::unblocked_assigned_for_agent(&state.db, agent_id).await?;
    Ok(Json(ActionItemsResponse {
        issues,
        pull_requests,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{RequestNew, RequestRespond};

    fn assert_bad_request_contains(err: AppError, needle: &str) {
        match err {
            AppError::BadRequest(msg) => assert!(
                msg.contains(needle),
                "expected BadRequest('{needle}'), got BadRequest('{msg}')"
            ),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn request_new_rejects_empty_payload() {
        let r = RequestNew {
            target_class: "claude".into(),
            target_type: None,
            payload: "".into(),
            channel_id: None,
        };
        assert_bad_request_contains(validate_request_new(&r).unwrap_err(), "payload required");
    }

    #[test]
    fn request_new_rejects_whitespace_only_payload() {
        for ws in ["   ", "\n", "\t\t", " \n \t "] {
            let r = RequestNew {
                target_class: "claude".into(),
                target_type: None,
                payload: ws.into(),
                channel_id: None,
            };
            assert_bad_request_contains(validate_request_new(&r).unwrap_err(), "payload required");
        }
    }

    #[test]
    fn request_new_accepts_non_empty_payload() {
        let r = RequestNew {
            target_class: "claude".into(),
            target_type: None,
            payload: "please refactor the loader".into(),
            channel_id: None,
        };
        validate_request_new(&r).expect("non-empty payload must validate");
    }

    #[test]
    fn request_respond_rejects_empty_response() {
        let r = RequestRespond {
            response: "".into(),
        };
        assert_bad_request_contains(
            validate_request_respond(&r).unwrap_err(),
            "response required",
        );
    }

    #[test]
    fn request_respond_rejects_whitespace_only_response() {
        let r = RequestRespond {
            response: "  \n  ".into(),
        };
        assert_bad_request_contains(
            validate_request_respond(&r).unwrap_err(),
            "response required",
        );
    }

    #[test]
    fn request_respond_accepts_non_empty_response() {
        let r = RequestRespond {
            response: "refactor landed in abc123".into(),
        };
        validate_request_respond(&r).expect("non-empty response must validate");
    }

    fn ok_new() -> ScheduledPromptNew {
        ScheduledPromptNew {
            scope: "team".into(),
            target_class: Some("claude".into()),
            target_kind: None,
            agent_id: None,
            ignore_pending_forgejo_work: false,
            name: "schedule-x".into(),
            prompt_text: "say hi".into(),
            interval_seconds: 60,
            enabled: true,
            ignore_inbox_state: false,
            weekly_safety_buffer_pct: 0,
            session_safety_buffer_pct: 0,
            context_clear_threshold_pct: None,
        }
    }

    #[test]
    fn scheduled_prompt_new_rejects_out_of_range_clear_threshold() {
        let mut n = ok_new();
        n.context_clear_threshold_pct = Some(150);
        assert_bad_request_contains(
            validate_scheduled_prompt_new(&n).unwrap_err(),
            "context_clear_threshold_pct must be 0..=100",
        );
        n.context_clear_threshold_pct = Some(-1);
        assert_bad_request_contains(
            validate_scheduled_prompt_new(&n).unwrap_err(),
            "context_clear_threshold_pct must be 0..=100",
        );
    }

    #[test]
    fn scheduled_prompt_new_accepts_clear_threshold_at_boundaries() {
        for v in [None, Some(0), Some(100)] {
            let mut n = ok_new();
            n.context_clear_threshold_pct = v;
            validate_scheduled_prompt_new(&n)
                .unwrap_or_else(|e| panic!("threshold {v:?} must validate: {e:?}"));
        }
    }

    #[test]
    fn scheduled_prompt_patch_rejects_out_of_range_clear_threshold() {
        let p = ScheduledPromptPatch {
            context_clear_threshold_pct: Some(101),
            ..Default::default()
        };
        assert_bad_request_contains(
            validate_scheduled_prompt_patch(&p).unwrap_err(),
            "context_clear_threshold_pct must be 0..=100",
        );
    }

    #[test]
    fn scheduled_prompt_patch_ignores_clearing_threshold_via_none() {
        let p = ScheduledPromptPatch {
            context_clear_threshold_pct: Some(0),
            ..Default::default()
        };
        validate_scheduled_prompt_patch(&p).expect("Some(0) (disabled) must validate on patch");
    }
}
