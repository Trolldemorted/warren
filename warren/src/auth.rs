use crate::db::Db;
use crate::entity::agent;
use crate::error::AppError;
use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{header, request::Parts, HeaderMap},
};
use chrono::{Duration, Utc};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use subtle::ConstantTimeEq;

pub const SESSION_COOKIE: &str = "warren_session";

/// Sliding-window threshold: when less than this fraction of the
/// **configured** TTL remains, the session's `expires_at` is bumped
/// and the browser cookie is re-issued with a fresh `Max-Age`. Keeps
/// active sessions alive indefinitely (until the operator rotates
/// `WARREN_ADMIN_PSK`, which invalidates all rows in `admin_sessions`
/// via a future hook — not wired here) without paying an `UPDATE` on
/// every single request.
///
/// Compared with "fraction of `expires_at - created_at`", this is
/// stable across refreshes: after one bump, `expires_at - now == TTL`,
/// so the next refresh is at least `ttl_hours * (1 - threshold)` away.
/// The fraction-of-total approach would re-fire on every request once
/// `created_at` was old enough, because the denominator grows with each
/// refresh while the numerator stays at TTL.
const REFRESH_THRESHOLD_FRAC: f64 = 0.5;

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
pub struct AgentAuth(pub agent::Model);

#[derive(Debug, Clone)]
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
        let ttl_hours = state.config.session_ttl_hours;

        let (cookie, bearer) = lookup_credentials(&parts.headers);

        if let Some(cookie) = cookie {
            // Cookie path: validate + slide forward when <50% of
            // `ttl_hours` remains. The cookie-refresh middleware in
            // `main.rs` runs the same check independently and is
            // responsible for the `Set-Cookie` response header; this
            // path is what gates the route.
            match validate_admin_session(&state.db, &cookie, ttl_hours).await? {
                SessionOutcome::Valid { refresh: _ } => {
                    return Ok(AuthContext::Admin(AdminUser));
                }
                SessionOutcome::Invalid => {}
            }
        }

        if let Some(token) = bearer {
            // Bearer-token path: no cookie to refresh, so we use a
            // separate validity-only helper. Even if the DB session is
            // close to expiring, an API client should keep working
            // until the token actually dies — the threshold+refresh
            // dance is a browser UX optimization, not a security
            // boundary.
            if validate_admin_session_valid_only(&state.db, &token).await? {
                return Ok(AuthContext::Admin(AdminUser));
            }
            if let Some(a) = lookup_agent_by_token(&state.db, &token).await? {
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

/// Outcome of validating an admin session cookie. `refresh=true` means
/// the row's `expires_at` was bumped forward by another `ttl_hours`
/// because less than `REFRESH_THRESHOLD_FRAC` of the original TTL
/// remained. The caller is then responsible for re-issuing the cookie
/// (the auth extractor stashes a `RefreshCookie` into request
/// extensions; the response middleware reads it and attaches the
/// `Set-Cookie` header).
pub enum SessionOutcome {
    Invalid,
    Valid { refresh: bool },
}

/// Validate an admin session and slide it forward when <50% of the
/// configured TTL remains. The bearer-token API path uses
/// `validate_admin_session_valid_only` instead, since API clients
/// don't carry a cookie to refresh.
pub async fn validate_admin_session(
    db: &Db,
    token: &str,
    ttl_hours: i64,
) -> Result<SessionOutcome, AppError> {
    use crate::entity::admin_session;
    let now = Utc::now();
    let found = admin_session::Entity::find()
        .filter(admin_session::Column::Token.eq(token.to_string()))
        .filter(admin_session::Column::ExpiresAt.gt(now))
        .one(db)
        .await?;
    let Some(model) = found else {
        return Ok(SessionOutcome::Invalid);
    };
    let ttl_window = Duration::hours(ttl_hours);
    let needs_refresh = should_refresh(model.expires_at - now, ttl_window);
    if needs_refresh {
        let new_expiry = now + ttl_window;
        let mut am: admin_session::ActiveModel = model.into();
        am.expires_at = Set(new_expiry);
        am.update(db).await?;
        Ok(SessionOutcome::Valid { refresh: true })
    } else {
        Ok(SessionOutcome::Valid { refresh: false })
    }
}

/// Pure-function form of the sliding-window threshold. Extracted so
/// the rule can be unit-tested without a live Postgres.
fn should_refresh(remaining: Duration, ttl_window: Duration) -> bool {
    if ttl_window <= Duration::zero() {
        return true;
    }
    let frac: f64 = (remaining.num_seconds() as f64) / (ttl_window.num_seconds() as f64);
    frac < REFRESH_THRESHOLD_FRAC
}

/// Pure validity check used by the Bearer-token auth path. Skips the
/// sliding-window refresh — API clients don't carry a cookie to roll
/// forward, and the threshold dance is a UX optimization, not a
/// security boundary.
pub async fn validate_admin_session_valid_only(db: &Db, token: &str) -> Result<bool, AppError> {
    use crate::entity::admin_session;
    let found = admin_session::Entity::find()
        .filter(admin_session::Column::Token.eq(token.to_string()))
        .filter(admin_session::Column::ExpiresAt.gt(Utc::now()))
        .one(db)
        .await?;
    Ok(found.is_some())
}

pub async fn lookup_agent_by_token(db: &Db, token: &str) -> Result<Option<agent::Model>, AppError> {
    Ok(agent::Entity::find()
        .filter(agent::Column::Authtoken.eq(token))
        .one(db)
        .await?)
}

/// Precedence-ordered credential lookup shared by both the in-process
/// `AuthContext` extractor and the `rabbit-lib` `AuthBackend` adapter.
/// Returns `(cookie, bearer)` — both may be present, callers pick the
/// first one that validates.
pub fn lookup_credentials(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    (read_session_cookie(headers), bearer_token(headers))
}

pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    //! Pin the sliding-window refresh rule without standing up Postgres.
    //! The contract: refresh when remaining < 50% of the configured TTL,
    //! using `expires_at - now` (NOT `expires_at - created_at`) as the
    //! remaining value. The latter would re-fire on every request once
    //! `created_at` was old enough because the denominator grows with
    //! each refresh while the numerator stays at TTL — see the
    //! `REFRESH_THRESHOLD_FRAC` doc comment for the worked example.
    use super::*;
    use chrono::Duration;

    fn ttl() -> Duration {
        Duration::hours(168) // 1 week, the new default
    }

    #[test]
    fn refresh_when_less_than_half_remaining() {
        // 80 hours remaining of a 168h TTL — under 50%.
        assert!(should_refresh(Duration::hours(80), ttl()));
    }

    #[test]
    fn no_refresh_when_more_than_half_remaining() {
        // 100 hours remaining — over 50%.
        assert!(!should_refresh(Duration::hours(100), ttl()));
    }

    #[test]
    fn no_refresh_at_exactly_half() {
        // Boundary: the threshold is `<`, not `<=`, so exactly 50%
        // doesn't trigger.
        assert!(!should_refresh(Duration::hours(84), ttl()));
    }

    #[test]
    fn refresh_just_under_half() {
        assert!(should_refresh(
            Duration::hours(83) + Duration::seconds(59 * 60 + 59),
            ttl()
        ));
    }

    #[test]
    fn zero_remaining_refreshes() {
        assert!(should_refresh(Duration::zero(), ttl()));
    }

    #[test]
    fn zero_ttl_always_refreshes() {
        // Defensive: a zero or negative TTL would otherwise NaN the
        // ratio. Always refresh in that case (operator misconfig —
        // they'll see sessions that never expire on the client).
        assert!(should_refresh(Duration::zero(), Duration::zero()));
        assert!(should_refresh(Duration::hours(10), Duration::zero()));
    }
}
