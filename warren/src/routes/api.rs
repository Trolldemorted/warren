use crate::auth::{AuthContext, SESSION_COOKIE};
use crate::entity::{agent, memo, request};
use crate::error::{AppError, AppResult};
use crate::ids::{new_agent_token, new_session_token};
use crate::models::{
    AgentNew, AgentPatch, LoginReq, LoginRes, MemoNew, RequestNew, RequestRespond,
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
        .route("/api/requests/incoming", get(api_incoming_requests))
        .route("/api/requests/:id", get(api_get_request))
        .route("/api/requests/:id/claim", post(api_claim_request))
        .route("/api/requests/:id/respond", post(api_respond_request))
        .route("/api/requests/:id/approve", post(api_approve_request))
        .route("/api/requests/:id/reject", post(api_reject_request))
        .route("/api/memos", get(api_list_memos).post(api_create_memo))
        .route("/api/memos/incoming", get(api_incoming_memos))
        .route("/api/memos/:id", get(api_get_memo))
        .route("/api/memos/:id/acknowledge", post(api_ack_memo))
        .route("/api/memos/:id/approve", post(api_approve_memo))
        .route("/api/memos/:id/reject", post(api_reject_memo))
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
    ctx.require_admin()?;
    let limit = q.limit.unwrap_or(50).clamp(1, 500) as u64;
    let offset = q.offset.unwrap_or(0).max(0) as u64;
    let rows =
        crate::db_ops::list_all_requests(&state.db, q.status.as_deref(), limit, offset).await?;
    Ok(Json(rows))
}

async fn api_create_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(new): Json<RequestNew>,
) -> AppResult<Json<request::Model>> {
    ctx.require_admin()?;
    if new.target_class.trim().is_empty() {
        return Err(AppError::BadRequest("target_class required".into()));
    }
    let r = crate::db_ops::create_request(&state.db, &new).await?;
    Ok(Json(r))
}

async fn api_incoming_requests(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<request::Model>>> {
    let (class, kind) = match &ctx {
        AuthContext::Admin(_) => {
            let rows =
                crate::db_ops::list_all_requests(&state.db, Some("approved"), 500, 0).await?;
            return Ok(Json(rows));
        }
        AuthContext::Agent(a) => (a.0.class.clone(), a.0.kind.clone()),
    };
    let rows = crate::db_ops::list_inbox(&state.db, &class, kind.as_deref()).await?;
    Ok(Json(rows))
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
            let claims_self = r.claimed_by.map(|cb| cb == a.0.id).unwrap_or(false);
            let in_inbox = r.status == "approved"
                && r.claimed_by.is_none()
                && r.target_class == a.0.class
                && (r.target_type.is_none() || r.target_type.as_deref() == a.0.kind.as_deref());
            if claims_self || in_inbox {
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
    crate::db_ops::set_request_status(&state.db, id, "pending", "approved").await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_reject_request(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::set_request_status(&state.db, id, "pending", "rejected").await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_list_memos(
    State(state): State<AppState>,
    ctx: AuthContext,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<memo::Model>>> {
    ctx.require_admin()?;
    let limit = q.limit.unwrap_or(50).clamp(1, 500) as u64;
    let offset = q.offset.unwrap_or(0).max(0) as u64;
    let rows = crate::db_ops::list_all_memos(&state.db, q.status.as_deref(), limit, offset).await?;
    Ok(Json(rows))
}

async fn api_create_memo(
    State(state): State<AppState>,
    ctx: AuthContext,
    Json(new): Json<MemoNew>,
) -> AppResult<Json<memo::Model>> {
    ctx.require_admin()?;
    if new.target_class.trim().is_empty() {
        return Err(AppError::BadRequest("target_class required".into()));
    }
    let m = crate::db_ops::create_memo(&state.db, &new).await?;
    Ok(Json(m))
}

async fn api_incoming_memos(
    State(state): State<AppState>,
    ctx: AuthContext,
) -> AppResult<Json<Vec<memo::Model>>> {
    let (class, kind, agent_id) = match &ctx {
        AuthContext::Admin(_) => {
            let rows = crate::db_ops::list_all_memos(&state.db, Some("approved"), 500, 0).await?;
            return Ok(Json(rows));
        }
        AuthContext::Agent(a) => (a.0.class.clone(), a.0.kind.clone(), a.0.id),
    };
    let rows = crate::db_ops::list_memo_inbox(&state.db, &class, kind.as_deref(), agent_id).await?;
    Ok(Json(rows))
}

async fn api_get_memo(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<Json<memo::Model>> {
    let m = crate::db_ops::get_memo(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    match &ctx {
        AuthContext::Admin(_) => Ok(Json(m)),
        AuthContext::Agent(a) => {
            let in_inbox = m.status == "approved"
                && m.target_class == a.0.class
                && (m.target_type.is_none() || m.target_type.as_deref() == a.0.kind.as_deref());
            if in_inbox {
                Ok(Json(m))
            } else {
                Err(AppError::Forbidden)
            }
        }
    }
}

async fn api_ack_memo(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let agent = ctx.require_agent()?;
    let m = crate::db_ops::get_memo(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if m.status != "approved" {
        return Err(AppError::Conflict("memo not approved".into()));
    }
    if m.target_class != agent.0.class
        || (m.target_type.is_some() && m.target_type.as_deref() != agent.0.kind.as_deref())
    {
        return Err(AppError::Forbidden);
    }
    crate::db_ops::acknowledge_memo(&state.db, id, agent.0.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_approve_memo(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::set_memo_status(&state.db, id, "pending", "approved").await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_reject_memo(
    State(state): State<AppState>,
    ctx: AuthContext,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    ctx.require_admin()?;
    crate::db_ops::set_memo_status(&state.db, id, "pending", "rejected").await?;
    Ok(StatusCode::NO_CONTENT)
}
