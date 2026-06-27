use crate::db::Db;
use crate::entity::agent;
use crate::error::AppError;
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{header, request::Parts, HeaderMap},
};
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, QueryFilter, Set,
    Statement,
};
use subtle::ConstantTimeEq;

pub const SESSION_COOKIE: &str = "warren_session";

pub fn read_session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(|s| s.trim())
        .find_map(|kv| {
            kv.strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(|v| v.to_string())
        })
}

#[derive(Debug, Clone)]
pub struct AdminUser;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AgentAuth(pub agent::Model);

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AuthContext {
    Admin(AdminUser),
    Agent(AgentAuth),
}

impl AuthContext {
    pub fn require_admin(&self) -> Result<AdminUser, AppError> {
        match self {
            AuthContext::Admin(a) => Ok(a.clone()),
            AuthContext::Agent(_) => Err(AppError::Forbidden),
        }
    }

    #[allow(dead_code)]
    pub fn require_agent(&self) -> Result<&AgentAuth, AppError> {
        match self {
            AuthContext::Agent(a) => Ok(a),
            AuthContext::Admin(_) => Err(AppError::Forbidden),
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for AuthContext
where
    S: Send + Sync,
    crate::AppState: axum::extract::FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        use axum::extract::FromRef;
        let state = crate::AppState::from_ref(state);

        if let Some(cookie) = parts
            .headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|h| h.to_str().ok())
            .flat_map(|s| s.split(';'))
            .map(|s| s.trim())
            .find_map(|kv| {
                kv.strip_prefix(&format!("{SESSION_COOKIE}="))
                    .map(|v| v.to_string())
            })
        {
            if validate_admin_session(&state.db, &cookie).await? {
                return Ok(AuthContext::Admin(AdminUser));
            }
        }

        if let Some(token) = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        {
            if validate_admin_session(&state.db, token).await? {
                return Ok(AuthContext::Admin(AdminUser));
            }
            if let Some(a) = lookup_agent_by_token(&state.db, token).await? {
                return Ok(AuthContext::Agent(AgentAuth(a)));
            }
        }

        Err(AppError::Unauthorized)
    }
}

pub fn psk_matches(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

pub async fn create_admin_session(db: &Db, token: &str, ttl_hours: i64) -> Result<(), AppError> {
    use crate::entity::admin_session;
    let expires = Utc::now() + Duration::hours(ttl_hours);
    let am = admin_session::ActiveModel {
        token: Set(token.to_string()),
        expires_at: Set(expires),
        ..Default::default()
    };
    am.insert(db).await?;
    Ok(())
}

pub async fn validate_admin_session(db: &Db, token: &str) -> Result<bool, AppError> {
    let sql =
        format!("SELECT 1 FROM admin_sessions WHERE token = '{token}' AND expires_at > now()");
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let row = db.query_one(stmt).await?;
    Ok(row.is_some())
}

pub async fn lookup_agent_by_token(db: &Db, token: &str) -> Result<Option<agent::Model>, AppError> {
    Ok(agent::Entity::find()
        .filter(agent::Column::Authtoken.eq(token))
        .one(db)
        .await?)
}
