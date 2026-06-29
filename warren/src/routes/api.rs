use crate::auth::{AuthContext, SESSION_COOKIE};
use crate::entity::{agent, request};
use crate::error::{AppError, AppResult};
use crate::ids::{new_agent_token, new_session_token};
use crate::models::{AgentNew, AgentPatch, LoginReq, LoginRes, RequestNew, RequestRespond};
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
        .route("/api/requests/:id", get(api_get_request))
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

async fn api_me(State(_state): State<AppState>, ctx: AuthContext) -> AppResult<Json<agent::Model>> {
    let auth = ctx.require_agent()?;
    Ok(Json(auth.0.clone()))
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
            let rows = crate::db_ops::list_requests_for_agent(
                &state.db,
                a.0.id,
                &a.0.class,
                a.0.kind.as_deref(),
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
        AuthContext::Admin(_) => (request::PENDING_RESPONSE_APPROVAL, None),
        AuthContext::Agent(a) => (request::PENDING_REQUEST_APPROVAL, Some(a.0.id)),
    };
    if new.target_class.trim().is_empty() {
        return Err(AppError::BadRequest("target_class required".into()));
    }
    let r = crate::db_ops::create_request(&state.db, &new, initial_status, sender_agent_id).await?;
    Ok(Json(r))
}

async fn api_get_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<request::Model>> {
    let r = crate::db_ops::get_request(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    match &ctx {
        AuthContext::Admin(_) => Ok(Json(r)),
        AuthContext::Agent(a) => {
            let sent_self = r.sender_agent_id.map(|s| s == a.0.id).unwrap_or(false);
            let claims_self = r.claimed_by.map(|cb| cb == a.0.id).unwrap_or(false);
            let in_inbox = r.status == request::PENDING_RESPONSE_APPROVAL
                && r.claimed_by.is_none()
                && r.target_class == a.0.class
                && (r.target_type.is_none() || r.target_type.as_deref() == a.0.kind.as_deref());
            if sent_self || claims_self || in_inbox {
                Ok(Json(r))
            } else {
                Err(AppError::Forbidden)
            }
        }
    }
}

async fn api_claim_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<request::Model>> {
    let agent = ctx.require_agent()?;
    let r = crate::db_ops::claim_request(&state.db, id, agent.0.id).await?;
    Ok(Json(r))
}

async fn api_respond_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
    Json(body): Json<RequestRespond>,
) -> AppResult<Json<request::Model>> {
    let agent = ctx.require_agent()?;
    let r = crate::db_ops::respond_to_request(&state.db, id, agent.0.id, &body.response).await?;
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
        request::PENDING_REQUEST_APPROVAL,
        request::PENDING_RESPONSE_APPROVAL,
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
        request::PENDING_REQUEST_APPROVAL,
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

fn parse_request_status(s: Option<&str>) -> AppResult<Option<i16>> {
    match s {
        None => Ok(None),
        Some("pending_request_approval") => Ok(Some(request::PENDING_REQUEST_APPROVAL)),
        Some("pending_response_approval") => Ok(Some(request::PENDING_RESPONSE_APPROVAL)),
        Some("done") => Ok(Some(request::DONE)),
        Some("rejected") => Ok(Some(request::REJECTED)),
        Some(other) => Err(AppError::BadRequest(format!("unknown status '{other}'"))),
    }
}
