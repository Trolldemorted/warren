use crate::AppState;
use axum::Router;
use tower_http::services::ServeDir;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new().fallback_service(ServeDir::new(state.config.static_dir.clone()))
}
