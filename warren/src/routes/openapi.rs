use axum::{http::header, response::IntoResponse, routing::get, Router};

const OPENAPI_YML: &str = include_str!("../../openapi.yml");

pub fn router() -> Router<crate::AppState> {
    Router::new().route(
        "/openapi.yml",
        get(|| async {
            ([(header::CONTENT_TYPE, "application/yaml")], OPENAPI_YML).into_response()
        }),
    )
}
