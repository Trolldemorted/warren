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
        AppError::BadGateway(e.to_string())
    }
}

impl AppError {
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
