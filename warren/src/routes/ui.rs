use crate::auth::SESSION_COOKIE;
use crate::error::{AppError, AppResult};
use crate::ids::new_session_token;
use crate::models::{AgentNew, AgentPatch, RequestNew};
use crate::templates::{
    AgentFormTemplate, AgentsTemplate, CommsInjectTemplate, CommsTemplate, Flash, LoginTemplate,
};
use crate::{auth, AppState};
use askama::Template;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_form))
        .route("/logout", post(logout))
        .route("/", get(root))
        .route("/agents", get(agents_page))
        .route("/agents/new", get(agent_new_page))
        .route("/agents", post(agent_create))
        .route("/agents/:id/edit", get(agent_edit_page))
        .route("/agents/:id", post(agent_update))
        .route("/agents/:id/delete", post(agent_delete))
        .route("/comms", get(comms_page))
        .route(
            "/comms/requests/new",
            get(inject_page_req).post(inject_create_req),
        )
        .route("/comms/requests/:id/approve", post(message_approve_request))
        .route("/comms/requests/:id/reject", post(message_reject_request))
        .route(
            "/comms/requests/:id/approve-response",
            post(message_approve_response),
        )
        .route(
            "/comms/requests/:id/reject-response",
            post(message_reject_response),
        )
        .route(
            "/comms/requests/:id/edit",
            get(message_edit_page).post(message_edit_save),
        )
}

async fn root() -> Redirect {
    Redirect::to("/agents")
}

async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if is_admin(&headers, &state).await {
        return Redirect::to("/agents").into_response();
    }
    let t = LoginTemplate {
        title: None,
        nav: None,
        flash: None,
        error: None,
    };
    render(t)
}

async fn login_form(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    match try_login(&state, &form.password).await {
        Ok(cookie) => {
            let mut resp = Redirect::to("/agents").into_response();
            resp.headers_mut().insert(header::SET_COOKIE, cookie);
            resp
        }
        Err(_) => {
            let t = LoginTemplate {
                title: None,
                nav: None,
                flash: Some(Flash::error("invalid password")),
                error: Some("invalid password".into()),
            };
            (
                StatusCode::UNAUTHORIZED,
                [
                    (header::SET_COOKIE, clear_cookie_value()),
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/html; charset=utf-8"),
                    ),
                ],
                render_to_string(t),
            )
                .into_response()
        }
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::read_session_cookie(&headers) {
        let _ = crate::db_ops::delete_admin_session(&state.db, &token).await;
    }
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, clear_cookie_value());
    resp
}

async fn try_login(state: &AppState, password: &str) -> AppResult<HeaderValue> {
    if !auth::psk_matches(password, &state.config.admin_psk) {
        return Err(AppError::Unauthorized);
    }
    let token = new_session_token();
    auth::create_admin_session(&state.db, &token, state.config.session_ttl_hours).await?;
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        state.config.session_ttl_hours * 3600
    );
    HeaderValue::from_str(&cookie).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))
}

fn clear_cookie_value() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
    ))
    .expect("static cookie value")
}

async fn is_admin(headers: &HeaderMap, state: &AppState) -> bool {
    if let Some(token) = auth::read_session_cookie(headers) {
        return auth::validate_admin_session(&state.db, &token)
            .await
            .unwrap_or(false);
    }
    false
}

async fn require_admin(state: &AppState, headers: &HeaderMap) -> AppResult<()> {
    if is_admin(headers, state).await {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

fn redirect_to_login() -> Response {
    Redirect::to("/login").into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

#[derive(Deserialize)]
struct AgentForm {
    name: String,
    class: String,
    #[serde(default)]
    kind: Option<String>,
    model: String,
    #[serde(default)]
    prompt: String,
}

async fn agents_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::list_agents(&state.db).await {
        Ok(agents) => {
            let t = AgentsTemplate {
                title: Some("Agents"),
                nav: Some("agents"),
                flash: None,
                agents,
            };
            render(t)
        }
        Err(e) => err_page(e),
    }
}

async fn agent_new_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let t = AgentFormTemplate {
        title: Some("New agent"),
        nav: Some("agents"),
        flash: None,
        agent: None,
        form_action: "/agents".into(),
    };
    render(t)
}

async fn agent_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AgentForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let new = AgentNew {
        name: form.name,
        class: form.class,
        kind: form.kind.filter(|s| !s.is_empty()),
        model: form.model,
        prompt: form.prompt,
    };
    match crate::db_ops::create_agent(&state.db, &new).await {
        Ok(agent) => {
            let id = agent.id;
            let t = AgentFormTemplate {
                title: Some("Agent created"),
                nav: Some("agents"),
                flash: Some(Flash::success("agent created")),
                agent: Some(agent),
                form_action: format!("/agents/{id}/edit"),
            };
            render(t)
        }
        Err(e) => err_page(e),
    }
}

async fn agent_edit_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::get_agent(&state.db, id).await {
        Ok(Some(agent)) => {
            let t = AgentFormTemplate {
                title: Some("Edit agent"),
                nav: Some("agents"),
                flash: None,
                agent: Some(agent),
                form_action: format!("/agents/{id}"),
            };
            render(t)
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => err_page(e),
    }
}

async fn agent_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(form): Form<AgentForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let patch = AgentPatch {
        name: Some(form.name),
        class: Some(form.class),
        model: Some(form.model),
        prompt: Some(form.prompt),
    };
    match crate::db_ops::update_agent(&state.db, id, &patch).await {
        Ok(_) => Redirect::to("/agents").into_response(),
        Err(e) => err_page(e),
    }
}

async fn agent_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::delete_agent(&state.db, id).await {
        Ok(_) => Redirect::to("/agents").into_response(),
        Err(e) => err_page(e),
    }
}

fn render<T: Template>(t: T) -> Response {
    t.render()
        .map(Html)
        .map(IntoResponse::into_response)
        .unwrap_or_else(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("render error: {e}"),
            )
                .into_response()
        })
}

fn render_to_string<T: Template>(t: T) -> String {
    t.render().unwrap_or_default()
}

fn err_page(e: AppError) -> Response {
    e.log();
    let (status, msg) = match &e {
        AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
        AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
        AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".into()),
        AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
    };
    (status, msg).into_response()
}

#[derive(Deserialize, Default)]
struct CommsQuery {
    status_req: Option<String>,
}

#[derive(Deserialize)]
struct InjectForm {
    target_class: String,
    #[serde(default)]
    target_type: Option<String>,
    payload: String,
    #[serde(default)]
    approved: Option<StrictBool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum StrictBool {
    On,
    True,
    #[serde(rename = "1")]
    One,
}

impl From<StrictBool> for bool {
    fn from(_: StrictBool) -> bool {
        true
    }
}

fn parse_payload(s: &str) -> serde_json::Value {
    serde_json::Value::String(s.to_string())
}

async fn comms_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CommsQuery>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let status_req_label = q
        .status_req
        .unwrap_or_else(|| "pending_request_approval".into());
    let status_req = parse_status_label(&status_req_label)
        .unwrap_or(crate::entity::request::PENDING_REQUEST_APPROVAL);
    let reqs = match crate::db_ops::list_all_requests(&state.db, Some(status_req), 200, 0).await {
        Ok(r) => r,
        Err(e) => return err_page(e),
    };
    let t = CommsTemplate {
        title: Some("Comms"),
        nav: Some("comms"),
        flash: None,
        requests: reqs,
        status_req: status_req_label,
        req_statuses: vec![
            "pending_request_approval".into(),
            "pending_response_approval".into(),
            "done".into(),
            "rejected".into(),
        ],
    };
    render(t)
}

fn parse_status_label(s: &str) -> Option<i16> {
    use crate::entity::request as r;
    match s {
        "pending_request_approval" => Some(r::PENDING_REQUEST_APPROVAL),
        "pending_response_approval" => Some(r::PENDING_RESPONSE_APPROVAL),
        "done" => Some(r::DONE),
        "rejected" => Some(r::REJECTED),
        _ => None,
    }
}

async fn inject_page_req(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (target_classes, target_kinds) =
        match crate::db_ops::distinct_agent_classes(&state.db).await {
            Ok(classes) => match crate::db_ops::distinct_agent_kinds(&state.db).await {
                Ok(kinds) => (classes, kinds),
                Err(e) => return err_page(e),
            },
            Err(e) => return err_page(e),
        };
    render(CommsInjectTemplate {
        title: Some("Inject request"),
        nav: Some("comms"),
        flash: None,
        target_classes,
        target_kinds,
    })
}

async fn inject_create_req(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<InjectForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let new = RequestNew {
        target_class: form.target_class,
        target_type: form.target_type.filter(|s| !s.is_empty()),
        payload: parse_payload(&form.payload),
        approved: form.approved.is_some(),
    };
    if let Err(e) = crate::db_ops::create_request(&state.db, &new).await {
        return err_page(e);
    }
    Redirect::to("/comms").into_response()
}

async fn message_approve_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::set_request_status(
        &state.db,
        id,
        crate::entity::request::PENDING_REQUEST_APPROVAL,
        crate::entity::request::PENDING_RESPONSE_APPROVAL,
    )
    .await
    {
        Ok(_) => Redirect::to("/comms").into_response(),
        Err(e) => err_page(e),
    }
}

async fn message_reject_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::set_request_status(
        &state.db,
        id,
        crate::entity::request::PENDING_REQUEST_APPROVAL,
        crate::entity::request::REJECTED,
    )
    .await
    {
        Ok(_) => Redirect::to("/comms").into_response(),
        Err(e) => err_page(e),
    }
}

async fn message_approve_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::accept_request_response(&state.db, id).await {
        Ok(_) => Redirect::to("/comms").into_response(),
        Err(e) => err_page(e),
    }
}

async fn message_reject_response(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::reject_request_response(&state.db, id).await {
        Ok(_) => Redirect::to("/comms").into_response(),
        Err(e) => err_page(e),
    }
}

#[derive(Template)]
#[template(path = "comms_edit.html")]
struct CommsEditTemplate {
    title: Option<&'static str>,
    nav: Option<&'static str>,
    flash: Option<Flash>,
    target_class: String,
    target_type: Option<String>,
    target_classes: Vec<String>,
    target_kinds: Vec<String>,
    payload: String,
    form_action: String,
}

async fn message_edit_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let req = match crate::db_ops::get_request(&state.db, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return err_page(e),
    };
    let target_classes = match crate::db_ops::distinct_agent_classes(&state.db).await {
        Ok(mut v) => {
            if !v.contains(&req.target_class) {
                v.insert(0, req.target_class.clone());
            }
            v
        }
        Err(e) => return err_page(e),
    };
    let target_kinds = match crate::db_ops::distinct_agent_kinds(&state.db).await {
        Ok(mut v) => {
            if let Some(t) = &req.target_type {
                if !v.contains(t) {
                    v.insert(0, t.clone());
                }
            }
            v
        }
        Err(e) => return err_page(e),
    };
    render(CommsEditTemplate {
        title: Some("Edit request"),
        nav: Some("comms"),
        flash: None,
        target_class: req.target_class,
        target_type: req.target_type,
        target_classes,
        target_kinds,
        payload: req.payload.to_string(),
        form_action: format!("/comms/requests/{id}/edit"),
    })
}

async fn message_edit_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(form): Form<InjectForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let target_type = form.target_type.filter(|s| !s.is_empty());
    let payload = parse_payload(&form.payload);
    match crate::db_ops::update_request_payload(
        &state.db,
        id,
        &form.target_class,
        target_type.as_deref(),
        &payload,
    )
    .await
    {
        Ok(_) => Redirect::to("/comms").into_response(),
        Err(e) => err_page(e),
    }
}
