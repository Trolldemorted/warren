use askama::Template;
use serde::Serialize;
use uuid::Uuid;

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "agents.html")]
pub struct AgentsTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agents: Vec<crate::entity::agent::Model>,
}

#[derive(Template)]
#[template(path = "agent_form.html")]
pub struct AgentFormTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agent: Option<crate::entity::agent::Model>,
    pub form_action: String,
}

#[derive(Template)]
#[template(path = "comms.html")]
pub struct CommsTemplate<'a> {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub rows: Vec<CommsRow<'a>>,
}

pub struct CommsRow<'a> {
    pub req: &'a crate::entity::request::Model,
    /// Display string for the source cell (e.g. `"claude/4-7"` or `"admin"`).
    pub source: String,
    /// Agent id backing the source cell, when the request was sent by an
    /// agent. `None` for admin-sent requests. Used to build a link to
    /// the agent's claude page.
    pub source_agent_id: Option<Uuid>,
    /// Agent id backing the target cell, when the request has been
    /// claimed by a specific agent. `None` for unclaimed requests
    /// (target is then just class+type, no specific agent). Used to
    /// build a link to the claiming agent's page.
    pub target_agent_id: Option<Uuid>,
    pub claimed_by_name: Option<String>,
    pub acknowledged_by_name: Option<String>,
}

#[derive(Template)]
#[template(path = "comms_inject.html")]
pub struct CommsInjectTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub target_classes: Vec<String>,
    pub target_kinds: Vec<String>,
}

#[derive(Template)]
#[template(path = "migrations.html")]
pub struct MigrationsTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub migrations: Vec<crate::db_ops::MigrationRow>,
}

#[derive(Template)]
#[template(path = "channels.html")]
pub struct ChannelsTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub channels: Vec<crate::entity::channel::Model>,
}

#[derive(Template)]
#[template(path = "agent_claude.html")]
pub struct AgentClaudeTemplate {
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agent: crate::entity::agent::Model,
    pub connected: bool,
    /// §D Milestone 5: true iff rabbit advertised a recorder URL on its
    /// most recent Hello envelope. Gates the "→ history" button in the
    /// actions aside so dead agents don't get a 404 link.
    pub recorder_enabled: bool,
}

/// §D Milestone 5: `/agent/:id/claude/history` — list view, one row per
/// recorded session, most recent first. `recorder_error` is `Some` when
/// the recorder URL is unknown OR the HTTP fetch failed; the template
/// renders a friendlier empty state instead of a 500.
#[derive(Template)]
#[template(path = "agent_claude_history_list.html")]
pub struct AgentClaudeHistoryListTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agent: crate::entity::agent::Model,
    pub sessions: Vec<crate::routes::recording::SessionRecordings>,
    pub recorder_error: Option<String>,
}

/// §D Milestone 5: `/agent/:id/claude/history/:session` — single-session
/// play page. Embeds an `<asciinema-player>` pointing at the recorder
/// URL for that session.
#[derive(Template)]
#[template(path = "agent_claude_history_play.html")]
pub struct AgentClaudeHistoryPlayTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agent: crate::entity::agent::Model,
    pub session_id: String,
    pub cast_url: String,
}

#[derive(Template)]
#[template(path = "agent_shell.html")]
/// §D Milestone 5: secondary bash PTY page. Same shape as
/// `AgentClaudeTemplate` — same xterm.js pane, different WS endpoint
/// and a smaller UI (no action buttons, just the live terminal).
pub struct AgentShellTemplate {
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agent: crate::entity::agent::Model,
    pub connected: bool,
}

#[derive(Template)]
#[template(path = "channel_form.html")]
pub struct ChannelFormTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub channel: Option<crate::entity::channel::Model>,
    pub form_action: String,
    pub classes: Vec<String>,
    pub kinds: Vec<String>,
    pub selected_sender_class: Option<String>,
    pub selected_sender_kind: Option<String>,
    pub selected_receiver_class: Option<String>,
    pub selected_receiver_kind: Option<String>,
    pub requires_request_approval: bool,
    pub requires_response_approval: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Flash {
    pub level: &'static str,
    pub message: String,
}

impl Flash {
    pub fn error(m: impl Into<String>) -> Self {
        Self {
            level: "error",
            message: m.into(),
        }
    }
    pub fn success(m: impl Into<String>) -> Self {
        Self {
            level: "success",
            message: m.into(),
        }
    }
}
