use crate::AppState;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower_http::services::ServeDir;

const DOCS_INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="UTF-8">
    <title>warren API</title>
    <link rel="stylesheet" type="text/css" href="./swagger-ui.css" />
    <link rel="stylesheet" type="text/css" href="index.css" />
    <link rel="icon" type="image/png" href="./favicon-32x32.png" sizes="32x32" />
    <link rel="icon" type="image/png" href="./favicon-16x16.png" sizes="16x16" />
  </head>
  <body>
    <div id="swagger-ui"></div>
    <script src="./swagger-ui-bundle.js" charset="UTF-8"></script>
    <script src="./swagger-ui-standalone-preset.js" charset="UTF-8"></script>
    <script src="./swagger-initializer.js" charset="UTF-8"></script>
  </body>
</html>
"#;

const DOCS_INITIALIZER_JS: &str = r##"window.onload = function() {
  window.ui = SwaggerUIBundle({
    url: "/openapi.yml",
    dom_id: "#swagger-ui",
    deepLinking: true,
    presets: [
      SwaggerUIBundle.presets.apis,
      SwaggerUIStandalonePreset
    ],
    plugins: [
      SwaggerUIBundle.plugins.DownloadUrl
    ],
    layout: "StandaloneLayout"
  });
};
"##;

pub fn router(state: AppState) -> Router<AppState> {
    let docs_dir = state.config.docs_dir.clone();
    Router::new()
        .route("/docs", get(docs_index))
        .route("/docs/", get(docs_index))
        .route("/docs/swagger-initializer.js", get(docs_initializer))
        .nest_service("/docs", ServeDir::new(docs_dir))
}

async fn docs_index() -> Response {
    html(DOCS_INDEX_HTML)
}

async fn docs_initializer() -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        DOCS_INITIALIZER_JS,
    )
        .into_response()
}

fn html(body: &'static str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}
