use crate::auth::{AgentAuth, AuthContext, SESSION_COOKIE};
use crate::db::Db;
use crate::entity::{agent, channel, request};
use crate::error::{AppError, AppResult};
use crate::ids::{new_agent_token, new_session_token};
use crate::models::{
    AgentNew, AgentPatch, ChannelNew, ChannelPatch, LoginReq, LoginRes, RequestNew, RequestRespond,
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
        .route(
            "/api/requests",
            get(api_list_requests).post(api_create_request),
        )
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
}

async fn build_request_not_found(db: &Db, agent: &AgentAuth, requested: Uuid) -> AppError {
    let rows = crate::db_ops::list_requests_for_agent(
        db,
        agent.0.id,
        &agent.0.class,
        agent.0.kind.as_deref(),
        false,
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
    include_done: Option<bool>,
}

async fn api_list_requests(
    State(state): State<AppState>,
    ctx: AuthContext,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<request::Model>>> {
    match &ctx {
        AuthContext::Admin(_) => {
            let limit = q.limit.unwrap_or(50).clamp(1, 500) as u64;
            let offset = q.offset.unwrap_or(0).max(0) as u64;
            let status = parse_request_status(q.status.as_deref())?;
            let rows = crate::db_ops::list_all_requests(&state.db, status, limit, offset).await?;
            Ok(Json(rows))
        }
        AuthContext::Agent(a) => {
            let _ = (q.limit, q.offset);
            let include_done = q.include_done.unwrap_or(false);
            let rows = crate::db_ops::list_requests_for_agent(
                &state.db,
                a.0.id,
                &a.0.class,
                a.0.kind.as_deref(),
                include_done,
            )
            .await?;
            Ok(Json(rows))
        }
    }
}

async fn api_create_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(new): Json<RequestNew>,
) -> AppResult<Json<request::Model>> {
    // Admin POSTs auto-skip request approval; agent POSTs go through review.
    let (initial_status, sender_agent_id) = match &ctx {
        AuthContext::Admin(_) => (request::AWAITING_AGENT_REQUEST_CLAIM, None),
        AuthContext::Agent(a) => (request::AWAITING_ADMIN_REQUEST_APPROVAL, Some(a.0.id)),
    };
    if new.target_class.trim().is_empty() {
        return Err(AppError::BadRequest("target_class required".into()));
    }
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
