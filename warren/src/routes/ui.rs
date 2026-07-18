use crate::auth::SESSION_COOKIE;
use crate::error::{AppError, AppResult};
use crate::ids::new_session_token;
use crate::models::{
    AgentForgejoConfigNew, AgentForgejoConfigPatch, AgentNew, AgentPatch, ChannelNew, RequestNew,
    ScheduledPromptPatch,
};
use crate::templates::{
    format_interval, outcome_badge, AgentClaudeTemplate, AgentFormTemplate, AgentOption, AgentRow,
    AgentShellTemplate, AgentsTemplate, ChannelFormTemplate, ChannelsTemplate, CommsInjectTemplate,
    CommsRow, CommsTemplate, Flash, LoginTemplate, MigrationsTemplate, ScheduledPromptFormTemplate,
    ScheduledPromptRow, ScheduledPromptRunRow, ScheduledPromptsTemplate,
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
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_form))
        .route("/logout", post(logout))
        .route("/", get(root))
        .route("/admin/agents", get(agents_page))
        .route("/admin/agents/new", get(agent_new_page))
        .route("/admin/agents", post(agent_create))
        .route("/admin/agents/:id/edit", get(agent_edit_page))
        .route("/admin/agents/:id", post(agent_update))
        .route("/admin/agents/:id/delete", post(agent_delete))
        .route("/admin/comms", get(comms_page))
        .route(
            "/admin/comms/requests/new",
            get(inject_page_req).post(inject_create_req),
        )
        .route(
            "/admin/comms/requests/:id/approve",
            post(message_approve_request),
        )
        .route(
            "/admin/comms/requests/:id/reject",
            post(message_reject_request),
        )
        .route(
            "/admin/comms/requests/:id/approve-response",
            post(message_approve_response),
        )
        .route(
            "/admin/comms/requests/:id/reject-response",
            post(message_reject_response),
        )
        .route(
            "/admin/comms/requests/:id/edit",
            get(message_edit_page).post(message_edit_save),
        )
        .route(
            "/admin/comms/requests/:id/delete",
            post(message_delete_request),
        )
        .route(
            "/admin/comms/requests/:id/set-status",
            post(message_set_status),
        )
        .route("/admin/migrations", get(migrations_page))
        .route("/admin/channels", get(channels_page))
        .route("/admin/channels/new", get(channel_new_page))
        .route("/admin/channels", post(channel_create))
        .route("/admin/channels/:id/edit", get(channel_edit_page))
        .route("/admin/channels/:id", post(channel_update))
        .route("/admin/channels/:id/delete", post(channel_delete))
        .route("/admin/scheduled-prompts", get(scheduled_prompts_page))
        .route(
            "/admin/scheduled-prompts/new",
            get(scheduled_prompt_new_page),
        )
        .route("/admin/scheduled-prompts", post(scheduled_prompt_create))
        .route(
            "/admin/scheduled-prompts/:id/edit",
            get(scheduled_prompt_edit_page),
        )
        .route(
            "/admin/scheduled-prompts/:id",
            post(scheduled_prompt_update),
        )
        .route(
            "/admin/scheduled-prompts/:id/delete",
            post(scheduled_prompt_delete),
        )
        .route(
            "/admin/scheduled-prompts/:id/run-now",
            post(scheduled_prompt_run_now),
        )
        .route("/agent/:id/claude", get(agent_claude_page))
        // §D Milestone 5: secondary bash PTY page.
        .route("/agent/:id/shell", get(agent_shell_page))
}

async fn root() -> Redirect {
    Redirect::to("/admin/agents")
}

async fn login_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if is_admin(&headers, &state).await {
        return Redirect::to("/admin/agents").into_response();
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
            let mut resp = Redirect::to("/admin/agents").into_response();
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
        return auth::validate_admin_session_valid_only(&state.db, &token)
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

/// Top-level fields parsed from the agent edit form. Forgejo config
/// keys are reconstructed manually from the flat map because
/// `serde_urlencoded` (which backs axum's `Form`) does not recurse into
/// `HashMap<String, Inner>` for `cfg[uuid][field]` syntax — the keys
/// reach us as literal strings.
struct AgentForm {
    name: String,
    class: String,
    kind: Option<String>,
    model: String,
    prompt: String,
    cfg: HashMap<Uuid, AgentForgejoConfigPatchForm>,
    new: HashMap<String, AgentForgejoConfigNewForm>,
}

#[derive(Default, Clone)]
struct AgentForgejoConfigPatchForm {
    base_url: String,
    owner: String,
    repo: String,
    forgejo_username: String,
    access_token: String,
}

#[derive(Default)]
struct AgentForgejoConfigNewForm {
    base_url: String,
    owner: String,
    repo: String,
    forgejo_username: String,
    access_token: String,
}

fn parse_agent_form(mut form: HashMap<String, String>) -> Result<AgentForm, AppError> {
    let name = form
        .remove("name")
        .ok_or_else(|| AppError::BadRequest("name required".into()))?;
    let class = form
        .remove("class")
        .ok_or_else(|| AppError::BadRequest("class required".into()))?;
    let model = form
        .remove("model")
        .ok_or_else(|| AppError::BadRequest("model required".into()))?;
    let prompt = form.remove("prompt").unwrap_or_default();
    let kind = form.remove("kind").filter(|v| !v.is_empty());

    let mut cfg: HashMap<Uuid, AgentForgejoConfigPatchForm> = HashMap::new();
    let mut new_map: HashMap<String, AgentForgejoConfigNewForm> = HashMap::new();
    for (key, value) in &form {
        // Split on the first '[' to separate the group prefix from the
        // "[<id>]" segment and the "[<field>]" segment.
        let (group, rest) = match key.split_once('[') {
            Some((g, r)) => (g, r),
            None => continue,
        };
        if group != "cfg" && group != "new" {
            continue;
        }
        let rest = rest.trim_end_matches(']');
        let (id_segment, field_segment) = match rest.split_once("][") {
            Some((id, f)) => (id, f),
            None => continue,
        };
        let field = field_segment.trim_end_matches(']');
        match group {
            "cfg" => {
                let uuid = match Uuid::parse_str(id_segment) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let entry = cfg.entry(uuid).or_default();
                set_patch_field(entry, field, value);
            }
            "new" => {
                let entry = new_map.entry(id_segment.to_string()).or_default();
                set_new_field(entry, field, value);
            }
            _ => {}
        }
    }
    Ok(AgentForm {
        name,
        class,
        kind,
        model,
        prompt,
        cfg,
        new: new_map,
    })
}

fn set_patch_field(p: &mut AgentForgejoConfigPatchForm, field: &str, value: &str) {
    match field {
        "base_url" => p.base_url = value.to_string(),
        "owner" => p.owner = value.to_string(),
        "repo" => p.repo = value.to_string(),
        "forgejo_username" => p.forgejo_username = value.to_string(),
        "access_token" => p.access_token = value.to_string(),
        _ => {}
    }
}

fn set_new_field(p: &mut AgentForgejoConfigNewForm, field: &str, value: &str) {
    match field {
        "base_url" => p.base_url = value.to_string(),
        "owner" => p.owner = value.to_string(),
        "repo" => p.repo = value.to_string(),
        "forgejo_username" => p.forgejo_username = value.to_string(),
        "access_token" => p.access_token = value.to_string(),
        _ => {}
    }
}

impl AgentForgejoConfigNewForm {
    fn is_empty(&self) -> bool {
        self.base_url.trim().is_empty()
            && self.owner.trim().is_empty()
            && self.repo.trim().is_empty()
            && self.forgejo_username.trim().is_empty()
            && self.access_token.is_empty()
    }
    fn into_model(self) -> AgentForgejoConfigNew {
        AgentForgejoConfigNew {
            base_url: self.base_url.trim().to_string(),
            owner: self.owner.trim().to_string(),
            repo: self.repo.trim().to_string(),
            forgejo_username: self.forgejo_username.trim().to_string(),
            access_token: self.access_token,
        }
    }
}

impl AgentForgejoConfigPatchForm {
    fn into_patch(self) -> AgentForgejoConfigPatch {
        let access_token = if self.access_token.is_empty() {
            None
        } else {
            Some(self.access_token.clone())
        };
        AgentForgejoConfigPatch {
            base_url: Some(self.base_url.trim().to_string()),
            owner: Some(self.owner.trim().to_string()),
            repo: Some(self.repo.trim().to_string()),
            forgejo_username: Some(self.forgejo_username.trim().to_string()),
            access_token,
        }
    }
}

#[derive(Deserialize, Default)]
struct AgentsPageQuery {
    /// `?reload=1|true` enables the auto-reload checkbox on the
    /// agents page; the inline script starts a 5s `location.reload()`
    /// when present. Anything else (or absent) renders the box
    /// unchecked and the page does not auto-reload.
    reload: Option<String>,
}

async fn agents_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AgentsPageQuery>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let reload = matches!(q.reload.as_deref(), Some("1") | Some("true"));
    match crate::db_ops::list_agents(&state.db).await {
        Ok(agents) => {
            // Per-row enrichment: look up live state from the rabbit-lib
            // registry (None = no rabbit currently registered → render as
            // "offline") and the agent's inbox count (requests actionable
            // right now for this agent's class+kind AND items the specific
            // agent has already claimed or must acknowledge).
            //
            // Done sequentially because the page renders at most O(agents)
            // rows and the queries are cheap. A `futures::join_all` would
            // be premature — the DB pool's connection cap is 10 by default
            // and admin agent counts are typically <50. If this ever
            // becomes a hot path, the natural move is one batched
            // `SELECT count(*) ... WHERE target_class = ANY(...)` plus a
            // single registry snapshot.
            let mut rows = Vec::with_capacity(agents.len());
            for a in agents {
                let status = state.live.registry.get(&a.id).map(|h| h.snapshot().state);
                let action_items = crate::db_ops::list_inbox_for_agent(
                    &state.db,
                    a.id,
                    &a.class,
                    a.kind.as_deref(),
                )
                .await
                .map(|rs| rs.len() as u64)
                .unwrap_or(0);
                // Forgejo unblocked-assigned counts across all of this
                // agent's configs. The shared helper iterates configs,
                // skips ones with empty forgejo_username, and tolerates
                // per-config errors (logged inside) — one broken repo
                // must not blank the dashboard.
                //
                // NB: this runs on every render. With the "Reload every
                // 5s" auto-refresh, a 50-agent fleet is 50 renderings /
                // 10 s, each doing K (config) × (1 issue list + N
                // issue-dependency lookups) HTTP calls. If that ever
                // becomes a hot path, memoize on (config_id, fetched_at)
                // with a short TTL — see
                // `forgejo::count_unblocked_assigned_for_agent`.
                let (forgejo_issues, forgejo_prs) =
                    crate::forgejo::count_unblocked_assigned_for_agent(&state.db, a.id)
                        .await
                        .unwrap_or((0, 0));
                rows.push(AgentRow {
                    agent: a,
                    status,
                    action_items,
                    forgejo_issues,
                    forgejo_prs,
                });
            }
            let t = AgentsTemplate {
                title: Some("Agents"),
                nav: Some("agents"),
                flash: None,
                reload,
                rows,
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
        form_action: "/admin/agents".into(),
        forgejo_configs: vec![],
    };
    render(t)
}

async fn agent_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(raw): Form<HashMap<String, String>>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let form = match parse_agent_form(raw) {
        Ok(f) => f,
        Err(e) => return err_page(e),
    };
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
                form_action: format!("/admin/agents/{id}/edit"),
                forgejo_configs: vec![],
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
            let forgejo_configs =
                match crate::db_ops::list_forgejo_configs_for_agent(&state.db, id).await {
                    Ok(v) => v,
                    Err(e) => return err_page(e),
                };
            let t = AgentFormTemplate {
                title: Some("Edit agent"),
                nav: Some("agents"),
                flash: None,
                agent: Some(agent),
                form_action: format!("/admin/agents/{id}"),
                forgejo_configs,
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
    Form(raw): Form<HashMap<String, String>>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let form = match parse_agent_form(raw) {
        Ok(f) => f,
        Err(e) => return err_page(e),
    };
    let patch = AgentPatch {
        name: Some(form.name),
        class: Some(form.class),
        kind: Some(form.kind.filter(|s| !s.is_empty())),
        model: Some(form.model),
        prompt: Some(form.prompt),
    };
    if let Err(e) = crate::db_ops::update_agent(&state.db, id, &patch).await {
        return err_page(e);
    }
    if let Err(e) = apply_forgejo_config_diff(&state.db, id, &form.cfg, form.new).await {
        return err_page(e);
    }
    Redirect::to("/admin/agents").into_response()
}

async fn apply_forgejo_config_diff(
    db: &crate::db::Db,
    agent_id: Uuid,
    cfg: &HashMap<Uuid, AgentForgejoConfigPatchForm>,
    new: HashMap<String, AgentForgejoConfigNewForm>,
) -> AppResult<()> {
    for (cfg_id, p) in cfg {
        if p.base_url.trim().is_empty()
            || p.owner.trim().is_empty()
            || p.repo.trim().is_empty()
            || p.forgejo_username.trim().is_empty()
        {
            return Err(AppError::BadRequest(format!(
                "forgejo config {cfg_id} has empty required field"
            )));
        }
    }
    for n in new.values() {
        if n.is_empty() {
            continue;
        }
        if n.base_url.trim().is_empty()
            || n.owner.trim().is_empty()
            || n.repo.trim().is_empty()
            || n.forgejo_username.trim().is_empty()
            || n.access_token.is_empty()
        {
            return Err(AppError::BadRequest(
                "forgejo config has empty required field".into(),
            ));
        }
    }

    let existing = crate::db_ops::list_forgejo_configs_for_agent(db, agent_id).await?;
    let submitted: HashSet<Uuid> = cfg.keys().copied().collect();

    for (cfg_id, p) in cfg {
        let patch: AgentForgejoConfigPatch = p.clone().into_patch();
        crate::db_ops::update_forgejo_config(db, *cfg_id, &patch).await?;
    }
    for old in &existing {
        if !submitted.contains(&old.id) {
            crate::db_ops::delete_forgejo_config(db, old.id).await?;
        }
    }
    for (_, n) in new {
        if n.is_empty() {
            continue;
        }
        let m = n.into_model();
        crate::db_ops::create_forgejo_config(db, agent_id, &m).await?;
    }
    Ok(())
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
        Ok(_) => Redirect::to("/admin/agents").into_response(),
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

#[derive(Deserialize)]
struct InjectForm {
    target_class: String,
    #[serde(default)]
    target_type: Option<String>,
    payload: String,
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

async fn comms_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let reqs = match crate::db_ops::list_all_requests(&state.db, None, 200, 0).await {
        Ok(r) => r,
        Err(e) => return err_page(e),
    };
    let agents = match crate::db_ops::list_agents(&state.db).await {
        Ok(a) => a,
        Err(e) => return err_page(e),
    };
    let agent_map: std::collections::HashMap<uuid::Uuid, &crate::entity::agent::Model> =
        agents.iter().map(|a| (a.id, a)).collect();
    let rows = reqs
        .iter()
        .map(|req| {
            let source = match req
                .sender_agent_id
                .as_ref()
                .and_then(|id| agent_map.get(id))
            {
                Some(a) => match &a.kind {
                    Some(k) => format!("{}/{}", a.class, k),
                    None => a.class.clone(),
                },
                None => "admin".to_string(),
            };
            let claimed_by_name = req
                .claimed_by
                .as_ref()
                .and_then(|id| agent_map.get(id).map(|a| a.name.clone()));
            let acknowledged_by_name = req
                .acknowledged_by
                .as_ref()
                .and_then(|id| agent_map.get(id).map(|a| a.name.clone()));
            CommsRow {
                req,
                source,
                source_agent_id: req.sender_agent_id,
                target_agent_id: req.claimed_by,
                claimed_by_name,
                acknowledged_by_name,
            }
        })
        .collect();
    let t = CommsTemplate {
        title: Some("Comms"),
        nav: Some("comms"),
        flash: None,
        rows,
    };
    render(t)
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
        payload: form.payload,
        channel_id: None,
    };
    // UI inject is admin-only — auto-skip request approval.
    if let Err(e) = crate::db_ops::create_request(
        &state.db,
        &new,
        crate::entity::request::AWAITING_AGENT_REQUEST_CLAIM,
        None,
    )
    .await
    {
        return err_page(e);
    }
    Redirect::to("/admin/comms").into_response()
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
        crate::entity::request::AWAITING_ADMIN_REQUEST_APPROVAL,
        crate::entity::request::AWAITING_AGENT_REQUEST_CLAIM,
    )
    .await
    {
        Ok(_) => Redirect::to("/admin/comms").into_response(),
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
        crate::entity::request::AWAITING_ADMIN_REQUEST_APPROVAL,
        crate::entity::request::REJECTED,
    )
    .await
    {
        Ok(_) => Redirect::to("/admin/comms").into_response(),
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
        Ok(_) => Redirect::to("/admin/comms").into_response(),
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
        Ok(_) => Redirect::to("/admin/comms").into_response(),
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
    response: String,
    status_label: &'static str,
    status_labels: Vec<String>,
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
            if let Some(t) = req.target_type.clone() {
                if !v.contains(&t) {
                    v.insert(0, t);
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
        target_class: req.target_class.clone(),
        target_type: req.target_type.clone(),
        target_classes,
        target_kinds,
        payload: req.payload.clone(),
        response: req.response.clone().unwrap_or_default(),
        status_label: req.status_label(),
        status_labels: crate::entity::request::STATUS_LABELS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        form_action: format!("/admin/comms/requests/{id}/edit"),
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
    let response = form.response.filter(|s| !s.is_empty());
    let status = match form.status.as_deref() {
        Some(label) => match parse_request_status_label(label) {
            Some(s) => s,
            None => return err_page(AppError::BadRequest(format!("unknown status '{label}'"))),
        },
        None => return err_page(AppError::BadRequest("status required".into())),
    };
    match crate::db_ops::update_request(
        &state.db,
        id,
        &form.target_class,
        target_type.as_deref(),
        &form.payload,
        response.as_deref(),
        status,
    )
    .await
    {
        Ok(_) => Redirect::to("/admin/comms").into_response(),
        Err(e) => err_page(e),
    }
}

async fn message_delete_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::delete_request(&state.db, id).await {
        Ok(_) => Redirect::to("/admin/comms").into_response(),
        Err(e) => err_page(e),
    }
}

#[derive(Deserialize)]
struct SetStatusForm {
    status: String,
}

async fn message_set_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(form): Form<SetStatusForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let label = form.status;
    let new_status = match parse_request_status_label(&label) {
        Some(s) => s,
        None => return err_page(AppError::BadRequest(format!("unknown status '{label}'"))),
    };
    match crate::db_ops::set_request_status_admin(&state.db, id, new_status).await {
        Ok(_) => Redirect::to("/admin/comms").into_response(),
        Err(e) => err_page(e),
    }
}

fn parse_request_status_label(label: &str) -> Option<i16> {
    use crate::entity::request::*;
    match label {
        "awaiting_admin_request_approval" => Some(AWAITING_ADMIN_REQUEST_APPROVAL),
        "awaiting_agent_request_claim" => Some(AWAITING_AGENT_REQUEST_CLAIM),
        "awaiting_agent_response" => Some(AWAITING_AGENT_RESPONSE),
        "awaiting_admin_response_approval" => Some(AWAITING_ADMIN_RESPONSE_APPROVAL),
        "awaiting_agent_response_acknowledge" => Some(AWAITING_AGENT_RESPONSE_ACKNOWLEDGE),
        "done" => Some(DONE),
        "rejected" => Some(REJECTED),
        _ => None,
    }
}

async fn migrations_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let migrations = match crate::db_ops::list_migrations(&state.db).await {
        Ok(m) => m,
        Err(e) => return err_page(e),
    };
    render(MigrationsTemplate {
        title: Some("Migrations"),
        nav: Some("migrations"),
        flash: None,
        migrations,
    })
}

#[derive(Deserialize)]
struct ChannelForm {
    sender_class: String,
    #[serde(default)]
    sender_kind: Option<String>,
    receiver_class: String,
    #[serde(default)]
    receiver_kind: Option<String>,
    description: String,
    #[serde(default)]
    requires_request_approval: bool,
    #[serde(default)]
    requires_response_approval: bool,
    /// Missing field → unchecked → disabled. The form template renders
    /// the box checked when `enabled` is true, so existing rows round-
    /// trip correctly and a user that disables a channel submits the
    /// form without the field.
    #[serde(default)]
    enabled: bool,
}

async fn channels_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::list_channels(&state.db).await {
        Ok(channels) => {
            let t = ChannelsTemplate {
                title: Some("Channels"),
                nav: Some("channels"),
                flash: None,
                channels,
            };
            render(t)
        }
        Err(e) => err_page(e),
    }
}

async fn channel_new_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (classes, kinds) = match load_class_kinds(&state.db).await {
        Ok(p) => p,
        Err(e) => return err_page(e),
    };
    render(ChannelFormTemplate {
        title: Some("New channel"),
        nav: Some("channels"),
        flash: None,
        channel: None,
        form_action: "/admin/channels".into(),
        classes,
        kinds,
        selected_sender_class: None,
        selected_sender_kind: None,
        selected_receiver_class: None,
        selected_receiver_kind: None,
        requires_request_approval: true,
        requires_response_approval: true,
        enabled: true,
    })
}

async fn channel_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ChannelForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (classes, kinds) = match load_class_kinds(&state.db).await {
        Ok(p) => p,
        Err(e) => return err_page(e),
    };
    if let Err(e) = validate_channel_form(&form, &classes, &kinds) {
        return err_page(e);
    }
    let new = ChannelNew {
        sender_class: form.sender_class,
        sender_kind: form.sender_kind.filter(|s| !s.is_empty()),
        receiver_class: form.receiver_class,
        receiver_kind: form.receiver_kind.filter(|s| !s.is_empty()),
        description: form.description,
        requires_request_approval: form.requires_request_approval,
        requires_response_approval: form.requires_response_approval,
        enabled: form.enabled,
    };
    match crate::db_ops::create_channel(&state.db, &new).await {
        Ok(_) => Redirect::to("/admin/channels").into_response(),
        Err(e) => err_page(e),
    }
}

async fn channel_edit_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (classes, kinds) = match load_class_kinds(&state.db).await {
        Ok(p) => p,
        Err(e) => return err_page(e),
    };
    match crate::db_ops::get_channel(&state.db, id).await {
        Ok(Some(channel)) => {
            let t = ChannelFormTemplate {
                title: Some("Edit channel"),
                nav: Some("channels"),
                flash: None,
                channel: Some(channel.clone()),
                form_action: format!("/admin/channels/{id}"),
                classes,
                kinds,
                selected_sender_class: Some(channel.sender_class.clone()),
                selected_sender_kind: channel.sender_kind.clone(),
                selected_receiver_class: Some(channel.receiver_class.clone()),
                selected_receiver_kind: channel.receiver_kind.clone(),
                requires_request_approval: channel.requires_request_approval,
                requires_response_approval: channel.requires_response_approval,
                enabled: channel.enabled,
            };
            render(t)
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => err_page(e),
    }
}

async fn channel_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(form): Form<ChannelForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (classes, kinds) = match load_class_kinds(&state.db).await {
        Ok(p) => p,
        Err(e) => return err_page(e),
    };
    if let Err(e) = validate_channel_form(&form, &classes, &kinds) {
        return err_page(e);
    }
    let patch = crate::models::ChannelPatch {
        sender_class: Some(form.sender_class),
        sender_kind: Some(form.sender_kind.filter(|s| !s.is_empty())),
        receiver_class: Some(form.receiver_class),
        receiver_kind: Some(form.receiver_kind.filter(|s| !s.is_empty())),
        description: Some(form.description),
        requires_request_approval: Some(form.requires_request_approval),
        requires_response_approval: Some(form.requires_response_approval),
        enabled: Some(form.enabled),
    };
    match crate::db_ops::update_channel(&state.db, id, &patch).await {
        Ok(_) => Redirect::to("/admin/channels").into_response(),
        Err(e) => err_page(e),
    }
}

async fn load_class_kinds(db: &crate::db::Db) -> AppResult<(Vec<String>, Vec<String>)> {
    let classes = crate::db_ops::distinct_agent_classes(db).await?;
    let kinds = crate::db_ops::distinct_agent_kinds(db).await?;
    Ok((classes, kinds))
}

/// Sibling of `load_class_kinds` that also pulls the agent list for
/// the scheduled-prompt form's agent-scope dropdown. The existing
/// channel form keeps the lighter `(classes, kinds)` shape so it
/// isn't disturbed by this addition.
async fn load_class_kinds_agents(
    db: &crate::db::Db,
) -> AppResult<(Vec<String>, Vec<String>, Vec<AgentOption>)> {
    let (classes, kinds) = load_class_kinds(db).await?;
    let agents = crate::db_ops::list_agents(db)
        .await?
        .into_iter()
        .map(|a| AgentOption {
            id: a.id.to_string(),
            name: a.name,
            class: a.class,
            kind: a.kind.unwrap_or_default(),
        })
        .collect();
    Ok((classes, kinds, agents))
}

fn validate_channel_form(
    form: &ChannelForm,
    classes: &[String],
    kinds: &[String],
) -> AppResult<()> {
    if form.description.trim().is_empty() {
        return Err(AppError::BadRequest("description required".into()));
    }
    if !classes.iter().any(|c| c == &form.sender_class) {
        return Err(AppError::BadRequest(format!(
            "unknown sender_class '{}'",
            form.sender_class
        )));
    }
    if !classes.iter().any(|c| c == &form.receiver_class) {
        return Err(AppError::BadRequest(format!(
            "unknown receiver_class '{}'",
            form.receiver_class
        )));
    }
    if let Some(k) = &form.sender_kind {
        if !k.is_empty() && !kinds.iter().any(|x| x == k) {
            return Err(AppError::BadRequest(format!("unknown sender_kind '{k}'")));
        }
    }
    if let Some(k) = &form.receiver_kind {
        if !k.is_empty() && !kinds.iter().any(|x| x == k) {
            return Err(AppError::BadRequest(format!("unknown receiver_kind '{k}'")));
        }
    }
    Ok(())
}

async fn channel_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::delete_channel(&state.db, id).await {
        Ok(_) => Redirect::to("/admin/channels").into_response(),
        Err(e) => err_page(e),
    }
}

async fn agent_claude_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::get_agent(&state.db, id).await {
        Ok(Some(agent)) => {
            let connected = state.live.registry.contains_key(&id);
            let initial_state = state.live.registry.get(&id).map(|h| h.snapshot().state);
            render(AgentClaudeTemplate {
                nav: Some("agents"),
                flash: None,
                agent,
                connected,
                initial_state,
                tui_cols: state.config.tui_cols,
                tui_rows: state.config.tui_rows,
            })
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => err_page(e),
    }
}

async fn agent_shell_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::get_agent(&state.db, id).await {
        Ok(Some(agent)) => {
            let connected = state.live.registry.contains_key(&id);
            render(AgentShellTemplate {
                nav: Some("agents"),
                flash: None,
                agent,
                connected,
                tui_cols: state.config.tui_cols,
                tui_rows: state.config.tui_rows,
            })
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => err_page(e),
    }
}

#[derive(Deserialize)]
struct ScheduledPromptForm {
    scope: Option<String>,
    target_class: String,
    target_kind: String,
    /// Raw agent selector; either an agent id (uuid string) or empty.
    agent_id: String,
    name: String,
    prompt_text: String,
    interval_seconds: String,
    enabled: Option<String>,
    ignore_inbox_state: Option<String>,
    ignore_pending_forgejo_work: Option<String>,
    weekly_safety_buffer_pct: String,
    session_safety_buffer_pct: String,
    context_clear_threshold_tokens: String,
}

fn scheduled_prompt_form_checkbox(s: Option<String>) -> bool {
    matches!(s.as_deref(), Some("true"))
}

fn parse_scheduled_prompt_form(
    form: ScheduledPromptForm,
) -> Result<ScheduledPromptFormParsed, AppError> {
    let scope = form
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("team");
    if scope != "team" && scope != "agent" {
        return Err(AppError::BadRequest(
            "scope must be 'team' or 'agent'".into(),
        ));
    }
    let target_class_trimmed = form.target_class.trim().to_string();
    let target_class_opt = if target_class_trimmed.is_empty() {
        None
    } else {
        Some(target_class_trimmed)
    };
    let target_kind_trimmed = form.target_kind.trim().to_string();
    let target_kind_opt = if target_kind_trimmed.is_empty() {
        None
    } else {
        Some(target_kind_trimmed)
    };
    let agent_id =
        {
            let t = form.agent_id.trim();
            if t.is_empty() {
                None
            } else {
                Some(Uuid::parse_str(t).map_err(|_| {
                    AppError::BadRequest(format!("agent_id '{t}' is not a valid uuid"))
                })?)
            }
        };
    match scope {
        "team" => {
            if target_class_opt.is_none() {
                return Err(AppError::BadRequest(
                    "target_class required for team scope".into(),
                ));
            }
            if agent_id.is_some() {
                return Err(AppError::BadRequest(
                    "agent_id must be empty for team scope".into(),
                ));
            }
        }
        "agent" => {
            if agent_id.is_none() {
                return Err(AppError::BadRequest(
                    "agent_id required for agent scope".into(),
                ));
            }
            if target_class_opt.is_some() || target_kind_opt.is_some() {
                return Err(AppError::BadRequest(
                    "target_class/target_kind must be empty for agent scope".into(),
                ));
            }
        }
        _ => unreachable!(),
    }
    if form.name.trim().is_empty() {
        return Err(AppError::BadRequest("name required".into()));
    }
    if form.prompt_text.trim().is_empty() {
        return Err(AppError::BadRequest("prompt_text required".into()));
    }
    let interval_seconds = form
        .interval_seconds
        .trim()
        .parse::<i64>()
        .map_err(|_| AppError::BadRequest("interval_seconds must be an integer".into()))?;
    if interval_seconds < 1 {
        return Err(AppError::BadRequest("interval_seconds must be >= 1".into()));
    }
    let weekly_safety_buffer_pct = form
        .weekly_safety_buffer_pct
        .trim()
        .parse::<i32>()
        .map_err(|_| AppError::BadRequest("weekly_safety_buffer_pct must be an integer".into()))?;
    let session_safety_buffer_pct = form
        .session_safety_buffer_pct
        .trim()
        .parse::<i32>()
        .map_err(|_| AppError::BadRequest("session_safety_buffer_pct must be an integer".into()))?;
    if !(0..=100).contains(&weekly_safety_buffer_pct) {
        return Err(AppError::BadRequest(
            "weekly_safety_buffer_pct must be 0..=100".into(),
        ));
    }
    if !(0..=100).contains(&session_safety_buffer_pct) {
        return Err(AppError::BadRequest(
            "session_safety_buffer_pct must be 0..=100".into(),
        ));
    }
    let context_clear_threshold_tokens_raw = form.context_clear_threshold_tokens.trim();
    let context_clear_threshold_tokens = if context_clear_threshold_tokens_raw.is_empty() {
        None
    } else {
        let v = context_clear_threshold_tokens_raw
            .parse::<i64>()
            .map_err(|_| {
                AppError::BadRequest("context_clear_threshold_tokens must be an integer".into())
            })?;
        if v < 0 {
            return Err(AppError::BadRequest(
                "context_clear_threshold_tokens must be a non-negative integer".into(),
            ));
        }
        Some(v)
    };
    Ok(ScheduledPromptFormParsed {
        scope: scope.to_string(),
        target_class: target_class_opt,
        target_kind: target_kind_opt,
        agent_id,
        name: form.name.trim().to_string(),
        prompt_text: form.prompt_text,
        interval_seconds,
        enabled: scheduled_prompt_form_checkbox(form.enabled),
        ignore_inbox_state: scheduled_prompt_form_checkbox(form.ignore_inbox_state),
        ignore_pending_forgejo_work: scheduled_prompt_form_checkbox(
            form.ignore_pending_forgejo_work,
        ),
        weekly_safety_buffer_pct,
        session_safety_buffer_pct,
        context_clear_threshold_tokens,
    })
}

struct ScheduledPromptFormParsed {
    scope: String,
    target_class: Option<String>,
    target_kind: Option<String>,
    agent_id: Option<Uuid>,
    name: String,
    prompt_text: String,
    interval_seconds: i64,
    enabled: bool,
    ignore_inbox_state: bool,
    ignore_pending_forgejo_work: bool,
    weekly_safety_buffer_pct: i32,
    session_safety_buffer_pct: i32,
    context_clear_threshold_tokens: Option<i64>,
}

async fn scheduled_prompts_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let prompts = match crate::db_ops::list_scheduled_prompts(&state.db).await {
        Ok(v) => v,
        Err(e) => return err_page(e),
    };

    let mut rows: Vec<ScheduledPromptRow> = Vec::with_capacity(prompts.len());
    for p in prompts {
        let interval_display = format_interval(p.interval_seconds);
        let last_outcome =
            match crate::db_ops::list_runs_for_scheduled_prompt(&state.db, p.id, 1).await {
                Ok(mut runs) => runs.pop().map(|r| r.outcome),
                Err(_) => None,
            };
        let last_outcome_badge = last_outcome.as_deref().map(outcome_badge);
        // Resolve the agent's display name for agent-scoped rows so
        // the index page can render `agent:<name>` instead of a raw
        // uuid. A missing agent means the FK ON DELETE CASCADE already
        // cleared the schedule, so this is best-effort and falls back
        // to the bare id.
        let (agent_name, target_class, target_kind_display) = match p.agent_id {
            Some(aid) => {
                let name = crate::db_ops::get_agent(&state.db, aid)
                    .await
                    .ok()
                    .flatten()
                    .map(|a| a.name)
                    .unwrap_or_else(|| aid.to_string());
                (Some(name), String::new(), "any".to_string())
            }
            None => (
                None,
                p.target_class.clone().unwrap_or_default(),
                p.target_kind.clone().unwrap_or_else(|| "any".to_string()),
            ),
        };
        rows.push(ScheduledPromptRow {
            target_class,
            target_kind_display,
            agent_name,
            prompt: p,
            interval_display,
            last_outcome,
            last_outcome_badge: last_outcome_badge.map(str::to_string),
        });
    }
    render(ScheduledPromptsTemplate {
        title: Some("Scheduled prompts"),
        nav: Some("scheduled_prompts"),
        flash: None,
        rows,
    })
}

async fn scheduled_prompt_new_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (classes, kinds, agents) = match load_class_kinds_agents(&state.db).await {
        Ok(v) => v,
        Err(e) => return err_page(e),
    };
    render(ScheduledPromptFormTemplate {
        title: Some("New schedule"),
        nav: Some("scheduled_prompts"),
        flash: None,
        prompt: None,
        form_action: "/admin/scheduled-prompts".into(),
        classes,
        kinds,
        agents,
        scope: "team".to_string(),
        target_class: String::new(),
        target_kind: String::new(),
        agent_id: None,
        name: String::new(),
        prompt_text: String::new(),
        interval_seconds: 3600,
        enabled: true,
        ignore_inbox_state: false,
        ignore_pending_forgejo_work: false,
        weekly_safety_buffer_pct: 0,
        session_safety_buffer_pct: 0,
        context_clear_threshold_tokens: None,
        runs: vec![],
    })
}

async fn scheduled_prompt_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ScheduledPromptForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let parsed = match parse_scheduled_prompt_form(form) {
        Ok(p) => p,
        Err(e) => return err_page(e),
    };
    let new = crate::models::ScheduledPromptNew {
        scope: parsed.scope,
        target_class: parsed.target_class,
        target_kind: parsed.target_kind,
        agent_id: parsed.agent_id,
        name: parsed.name,
        prompt_text: parsed.prompt_text,
        interval_seconds: parsed.interval_seconds,
        enabled: parsed.enabled,
        ignore_inbox_state: parsed.ignore_inbox_state,
        ignore_pending_forgejo_work: parsed.ignore_pending_forgejo_work,
        weekly_safety_buffer_pct: parsed.weekly_safety_buffer_pct,
        session_safety_buffer_pct: parsed.session_safety_buffer_pct,
        context_clear_threshold_tokens: parsed.context_clear_threshold_tokens,
    };
    match crate::db_ops::create_scheduled_prompt(&state.db, &new).await {
        Ok(_) => Redirect::to("/admin/scheduled-prompts").into_response(),
        Err(e) => err_page(e),
    }
}

async fn scheduled_prompt_edit_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let (classes, kinds, agents) = match load_class_kinds_agents(&state.db).await {
        Ok(v) => v,
        Err(e) => return err_page(e),
    };
    match crate::db_ops::get_scheduled_prompt(&state.db, id).await {
        Ok(Some(p)) => {
            let runs = match crate::db_ops::list_runs_for_scheduled_prompt(&state.db, id, 50).await
            {
                Ok(v) => v
                    .into_iter()
                    .map(|r| ScheduledPromptRunRow {
                        outcome_badge: outcome_badge(&r.outcome).to_string(),
                        run: r,
                    })
                    .collect(),
                Err(_) => vec![],
            };
            render(ScheduledPromptFormTemplate {
                title: Some("Edit schedule"),
                nav: Some("scheduled_prompts"),
                flash: None,
                prompt: Some(p.clone()),
                form_action: format!("/admin/scheduled-prompts/{id}"),
                classes,
                kinds,
                agents,
                scope: p.scope.clone(),
                target_class: p.target_class.clone().unwrap_or_default(),
                target_kind: p.target_kind.clone().unwrap_or_default(),
                agent_id: p.agent_id,
                name: p.name,
                prompt_text: p.prompt_text,
                interval_seconds: p.interval_seconds,
                enabled: p.enabled,
                ignore_inbox_state: p.ignore_inbox_state,
                ignore_pending_forgejo_work: p.ignore_pending_forgejo_work,
                weekly_safety_buffer_pct: p.weekly_safety_buffer_pct,
                session_safety_buffer_pct: p.session_safety_buffer_pct,
                context_clear_threshold_tokens: p.context_clear_threshold_tokens,
                runs,
            })
        }
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => err_page(e),
    }
}

async fn scheduled_prompt_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Form(form): Form<ScheduledPromptForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let parsed = match parse_scheduled_prompt_form(form) {
        Ok(p) => p,
        Err(e) => return err_page(e),
    };
    let patch = ScheduledPromptPatch {
        scope: None,
        name: Some(parsed.name),
        prompt_text: Some(parsed.prompt_text),
        interval_seconds: Some(parsed.interval_seconds),
        enabled: Some(parsed.enabled),
        ignore_inbox_state: Some(parsed.ignore_inbox_state),
        weekly_safety_buffer_pct: Some(parsed.weekly_safety_buffer_pct),
        session_safety_buffer_pct: Some(parsed.session_safety_buffer_pct),
        context_clear_threshold_tokens: parsed.context_clear_threshold_tokens,
        ignore_pending_forgejo_work: Some(parsed.ignore_pending_forgejo_work),
    };
    match crate::db_ops::update_scheduled_prompt(&state.db, id, &patch).await {
        Ok(_) => Redirect::to("/admin/scheduled-prompts").into_response(),
        Err(e) => err_page(e),
    }
}

async fn scheduled_prompt_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    match crate::db_ops::delete_scheduled_prompt(&state.db, id).await {
        Ok(_) => Redirect::to("/admin/scheduled-prompts").into_response(),
        Err(e) => err_page(e),
    }
}

async fn scheduled_prompt_run_now(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return redirect_to_login();
    }
    let prompt = match crate::db_ops::get_scheduled_prompt(&state.db, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => return err_page(e),
    };
    if !prompt.enabled {
        return err_page(AppError::Conflict("schedule is disabled".into()));
    }
    let arc = std::sync::Arc::new(state);
    if let Err(e) = crate::scheduler::fire_prompt(arc, prompt).await {
        log::error!("scheduler: run-now failed: {e:?}");
    }
    Redirect::to("/admin/scheduled-prompts").into_response()
}
