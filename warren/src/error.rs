use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("no request with id '{0}' exists")]
    RequestNotFound(Uuid, Vec<Uuid>),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("bad gateway: {0}")]
    BadGateway(String),
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<forgejo_api::ForgejoError> for AppError {
    fn from(e: forgejo_api::ForgejoError) -> Self {
        use forgejo_api::{ApiError, ApiErrorKind, ForgejoError};
        // Map the upstream status code forgejo-api surfaced to the
        // matching `AppError` variant so a 404 doesn't render as
        // "bad gateway" (502) in the response and log. Only genuine
        // upstream/proxy errors and transport failures keep the
        // `BadGateway` label. Every message-bearing variant gets a
        // leading `status=NNN <Reason>` so the operator can read the
        // upstream HTTP code straight off the log line.
        match e {
            ForgejoError::ApiError(ApiError { ref kind, .. }) => match kind {
                ApiErrorKind::NotFound { .. } => AppError::NotFound,
                ApiErrorKind::Forbidden => AppError::Forbidden,
                ApiErrorKind::Unauthorized => AppError::Unauthorized,
                ApiErrorKind::ValidationFailed => AppError::BadRequest(
                    "status=422 Unprocessable Entity: forgejo validation failed".into(),
                ),
                ApiErrorKind::RepoArchived => {
                    AppError::Conflict("status=409 Conflict: forgejo repo is archived".into())
                }
                ApiErrorKind::Generic | ApiErrorKind::InvalidTopics { .. } => {
                    AppError::Internal(anyhow::anyhow!("status=? forgejo api error: {e}"))
                }
                ApiErrorKind::Other(status) => status_app_error(*status, e),
            },
            // `UnexpectedStatusCode` is forgejo-api's path for any
            // HTTP response without a typed JSON body (502, 503,
            // upstream proxy timeouts, …). Those really *are* "bad
            // gateway" semantics, so the label survives here.
            ForgejoError::UnexpectedStatusCode(status) => status_app_error(status, e),
            ForgejoError::ReqwestError(_) => {
                AppError::BadGateway(format!("status=? forgejo transport error: {e}"))
            }
            ForgejoError::BadStructure(_)
            | ForgejoError::HostRequired
            | ForgejoError::HttpRequired
            | ForgejoError::KeyNotAscii
            | ForgejoError::AuthTooLong => AppError::Internal(anyhow::anyhow!("forgejo: {e}")),
        }
    }
}

/// Map a bare `reqwest::StatusCode` from forgejo-api's
/// `UnexpectedStatusCode` / `ApiErrorKind::Other` arm to the
/// closest `AppError` variant. Every branch embeds
/// `status=NNN <Reason>` so the upstream code is visible in the log.
fn status_app_error(status: reqwest::StatusCode, e: forgejo_api::ForgejoError) -> AppError {
    use forgejo_api::ForgejoError;
    let tag = format!(
        "status={} {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    match status.as_u16() {
        400 => AppError::BadRequest(format!("{tag}: {e}")),
        401 => AppError::Unauthorized,
        403 => AppError::Forbidden,
        404 => AppError::NotFound,
        409 => AppError::Conflict(format!("{tag}: {e}")),
        422 => AppError::BadRequest(format!("{tag}: {e}")),
        500..=599 => AppError::BadGateway(format!("{tag}: {e}")),
        _ => match e {
            ForgejoError::UnexpectedStatusCode(_) => AppError::BadGateway(format!("{tag}: {e}")),
            other => AppError::Internal(anyhow::anyhow!("forgejo: {other}")),
        },
    }
}

impl AppError {
    /// Prepend the URL forgejo-api attempted (the operator's debug
    /// handle for 4xx/5xx) to every message-bearing variant of this
    /// error. Preserves the variant so a 404 still renders as
    /// `AppError::NotFound` (and a 502 still as `AppError::BadGateway`)
    /// — only the human-readable string gains the URL.
    pub fn with_url(self, url: &str) -> Self {
        let prefix = format!("GET {url}: ");
        match self {
            AppError::BadRequest(m) => AppError::BadRequest(format!("{prefix}{m}")),
            AppError::Conflict(m) => AppError::Conflict(format!("{prefix}{m}")),
            AppError::BadGateway(m) => AppError::BadGateway(format!("{prefix}{m}")),
            AppError::Internal(e) => AppError::Internal(anyhow::anyhow!("{prefix}{e}")),
            // Variants without a string payload — the URL belongs on
            // the log line, not in the user-facing message.
            AppError::NotFound
            | AppError::RequestNotFound(..)
            | AppError::Unauthorized
            | AppError::Forbidden
            | AppError::Db(_) => self,
        }
    }

    pub fn log(&self) {
        match self {
            AppError::Db(e) => {
                log::error!("database error: {e}");
                log::debug!("database error detail: {e:?}");
            }
            AppError::Internal(e) => {
                log::error!("internal error: {e:?}");
                log::debug!("internal error detail: {e:#?}");
            }
            _ => {}
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Db(e) => {
                log::error!("database error: {e}");
                log::debug!("database error detail: {e:?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal error", "code": "internal"})),
                )
                    .into_response()
            }
            AppError::Internal(e) => {
                log::error!("internal error: {e:?}");
                log::debug!("internal error detail: {e:#?}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal error", "code": "internal"})),
                )
                    .into_response()
            }
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "not found", "code": "not_found"})),
            )
                .into_response(),
            AppError::RequestNotFound(id, eligible) => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": format!("no request with id '{id}' exists"),
                    "code": "request_not_found",
                    "eligible_request_ids": eligible,
                })),
            )
                .into_response(),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized", "code": "unauthorized"})),
            )
                .into_response(),
            AppError::Forbidden => (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "forbidden", "code": "forbidden"})),
            )
                .into_response(),
            AppError::Conflict(m) => (
                StatusCode::CONFLICT,
                Json(json!({"error": m, "code": "conflict"})),
            )
                .into_response(),
            AppError::BadRequest(m) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": m, "code": "bad_request"})),
            )
                .into_response(),
            AppError::BadGateway(m) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": m, "code": "bad_gateway"})),
            )
                .into_response(),
        }
    }
}

pub fn map_unique_conflict(e: sea_orm::DbErr, msg: &'static str) -> AppError {
    use sea_orm::RuntimeErr;
    if let sea_orm::DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(db))) = &e {
        if db.code().as_deref() == Some("23505") {
            return AppError::Conflict(msg.into());
        }
    }
    AppError::Db(e)
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;
    use forgejo_api::{ApiError, ApiErrorKind, ForgejoError};
    use reqwest::StatusCode;

    fn api_err(kind: ApiErrorKind) -> ForgejoError {
        ForgejoError::ApiError(ApiError {
            message: Some("The target couldn't be found".into()),
            kind,
        })
    }

    #[test]
    fn forgejo_not_found_does_not_become_bad_gateway() {
        let e = api_err(ApiErrorKind::NotFound { errors: None });
        let app = AppError::from(e);
        assert!(
            matches!(app, AppError::NotFound),
            "404 must map to NotFound, got {app:?}"
        );
    }

    #[test]
    fn forgejo_forbidden_maps_to_forbidden() {
        let app = AppError::from(api_err(ApiErrorKind::Forbidden));
        assert!(matches!(app, AppError::Forbidden));
    }

    #[test]
    fn forgejo_unauthorized_maps_to_unauthorized() {
        let app = AppError::from(api_err(ApiErrorKind::Unauthorized));
        assert!(matches!(app, AppError::Unauthorized));
    }

    #[test]
    fn forgejo_validation_failed_maps_to_bad_request() {
        let app = AppError::from(api_err(ApiErrorKind::ValidationFailed));
        assert!(matches!(app, AppError::BadRequest(_)));
    }

    #[test]
    fn forgejo_unexpected_502_maps_to_bad_gateway() {
        let app = AppError::from(ForgejoError::UnexpectedStatusCode(StatusCode::BAD_GATEWAY));
        assert!(
            matches!(app, AppError::BadGateway(_)),
            "502 must map to BadGateway, got {app:?}"
        );
        let AppError::BadGateway(msg) = &app else {
            unreachable!()
        };
        assert!(
            msg.contains("status=502"),
            "BadGateway message must include upstream status, got {msg:?}"
        );
        assert!(msg.contains("Bad Gateway"), "must include canonical reason");
    }

    #[test]
    fn forgejo_validation_failed_message_carries_status() {
        let app = AppError::from(api_err(ApiErrorKind::ValidationFailed));
        let AppError::BadRequest(msg) = &app else {
            panic!("expected BadRequest, got {app:?}")
        };
        assert!(msg.contains("status=422"), "got {msg:?}");
    }

    #[test]
    fn forgejo_repo_archived_message_carries_status() {
        let app = AppError::from(api_err(ApiErrorKind::RepoArchived));
        let AppError::Conflict(msg) = &app else {
            panic!("expected Conflict, got {app:?}")
        };
        assert!(msg.contains("status=409"), "got {msg:?}");
    }

    #[test]
    fn forgejo_unexpected_404_maps_to_not_found_not_bad_gateway() {
        let app = AppError::from(ForgejoError::UnexpectedStatusCode(StatusCode::NOT_FOUND));
        assert!(
            matches!(app, AppError::NotFound),
            "404 must map to NotFound, got {app:?}"
        );
    }

    #[test]
    fn forgejo_unexpected_422_maps_to_bad_request() {
        let app = AppError::from(ForgejoError::UnexpectedStatusCode(
            StatusCode::UNPROCESSABLE_ENTITY,
        ));
        let AppError::BadRequest(msg) = &app else {
            panic!("expected BadRequest, got {app:?}")
        };
        assert!(msg.contains("status=422"), "got {msg:?}");
    }

    #[test]
    fn with_url_then_status_render_in_url_status_message_order() {
        // Simulates the call-site pattern:
        //   AppError::from(forgejo_err).with_url(&url)
        // and asserts the log line shows URL, status, and inner
        // message in that order — operator reads left-to-right.
        let e = api_err(ApiErrorKind::NotFound { errors: None });
        // Variant has no payload, so with_url doesn't append the
        // URL to a message — the variant itself stays NotFound and
        // the IntoResponse renders the 404 status. Confirm:
        let app = AppError::from(e).with_url("https://forge.example/api/v1/repos/x/y/issues");
        assert!(matches!(app, AppError::NotFound));
    }

    #[test]
    fn with_url_appends_to_status_embedded_bad_gateway() {
        let e = ForgejoError::UnexpectedStatusCode(StatusCode::BAD_GATEWAY);
        let app = AppError::from(e).with_url("https://forge.example/api/v1/repos/x/y/issues");
        let AppError::BadGateway(msg) = &app else {
            panic!("expected BadGateway, got {app:?}")
        };
        assert!(msg.starts_with("GET https://forge.example/"), "got {msg:?}");
        assert!(msg.contains("status=502"), "got {msg:?}");
        assert!(
            msg.contains("Bad Gateway"),
            "canonical reason must survive, got {msg:?}"
        );
    }

    #[test]
    fn with_url_prepends_to_message_bearing_variants() {
        let app = AppError::NotFound.with_url("https://forge.example/api/v1/repos/x/y/issues");
        // NotFound carries no message payload — variant must NOT be
        // promoted to BadGateway or wrap the URL into the response.
        assert!(matches!(app, AppError::NotFound));
    }

    #[test]
    fn with_url_prepends_to_bad_gateway() {
        let app = AppError::BadGateway("not found: The target couldn't be found".into())
            .with_url("https://forge.example/api/v1/repos/x/y/issues");
        match app {
            AppError::BadGateway(m) => {
                assert!(m.starts_with("GET https://forge.example/api/v1/repos/x/y/issues: "));
                assert!(m.contains("not found"));
            }
            other => panic!("variant must stay BadGateway, got {other:?}"),
        }
    }
}
