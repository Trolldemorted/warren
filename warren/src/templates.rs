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
    /// `?reload=1|true` on the request URL — the page re-renders the
    /// auto-reload checkbox checked and the inline script starts a
    /// 5-second `location.reload()` interval. Defaults to false.
    pub reload: bool,
    /// One row per agent, bundling the persisted model with the two
    /// live-derived columns (status + action items). Parallel
    /// `Vec<...>` fields would force Askama's indexing dance; a single
    /// struct lets the template iterate `rows` and pull each piece by
    /// name. The modals at the bottom of the template iterate the
    /// inner `agent` field, so the model stays accessible.
    pub rows: Vec<AgentRow>,
}

/// Per-row enrichment for the agents index page. `status` is `None`
/// when no rabbit is currently registered for the agent (the template
/// renders this as "offline" — distinct from the typed `AgentState::Dead`
/// which means "registered but not running"). `action_items` counts
/// requests actionable right now: claims for this agent's class+kind,
/// already-claimed requests this agent must respond to, and requests
/// this agent must acknowledge (per `db_ops::list_inbox_for_agent`).
/// `forgejo_issues` and `forgejo_prs` count the open items currently
/// assigned to this agent across all of its forgejo configs where no
/// dependency is still open (per `forgejo::fetch_unblocked_assigned`).
/// Per-config fetch failures are logged and counted as zero — see the
/// note in `routes::ui::agents_page` before adding caching.
pub struct AgentRow {
    pub agent: crate::entity::agent::Model,
    pub status: Option<rabbit_lib::wire::AgentState>,
    pub action_items: u64,
    pub forgejo_issues: u64,
    pub forgejo_prs: u64,
    /// Most-recent per-config error message for this agent, or `None`
    /// if every config succeeded (or there were no configs). When
    /// `Some`, the agents dashboard renders an `err` badge with the
    /// full message(s) available in the title attribute. Truncated to
    /// ~120 chars per config so the cell stays compact.
    pub forgejo_error: Option<String>,
}

#[derive(Template)]
#[template(path = "agent_form.html")]
pub struct AgentFormTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub agent: Option<crate::entity::agent::Model>,
    pub form_action: String,
    pub forgejo_configs: Vec<crate::entity::agent_forgejo_config::Model>,
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
    /// Latest `AgentState` from the live registry, used to render the
    /// initial status badge before the WebSocket delivers its first
    /// `state` envelope. `None` when no rabbit is registered, which
    /// the template renders as "offline". Mirrors the per-row
    /// `AgentRow::status` used on the agents index page so the two
    /// surfaces can't disagree on the same agent.
    pub initial_state: Option<rabbit_lib::wire::AgentState>,
    pub tui_cols: u16,
    pub tui_rows: u16,
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
    pub tui_cols: u16,
    pub tui_rows: u16,
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
    pub enabled: bool,
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

#[derive(Template)]
#[template(path = "scheduled_prompts.html")]
pub struct ScheduledPromptsTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub rows: Vec<ScheduledPromptRow>,
}

/// One row of the scheduled-prompts index. The address display is
/// pre-resolved by the handler:
///   - team scope → `target_class:target_kind_display` (e.g. `claude:opus`).
///   - agent scope → `agent_name` (the linked agent's display name;
///     falls back to the raw uuid at the handler if the FK was deleted).
///
/// The badge class for the most-recent run's `outcome` is also
/// precomputed — Askama can't call free functions.
///
/// All string-shaped fields are pre-rendered to `String` so Askama's
/// `Display` bound on `MarkupDisplay::new(&field)` doesn't have to
/// recurse through `Option<String>` (which the derive macro is not
/// always able to satisfy for arbitrary structs).
pub struct ScheduledPromptRow {
    pub prompt: crate::entity::scheduled_prompt::Model,
    pub target_class: String,
    pub target_kind_display: String,
    pub agent_name: Option<String>,
    pub interval_display: String,
    pub last_outcome: Option<String>,
    pub last_outcome_badge: Option<String>,
}

/// Per-agent entry in the form's `<select>` for the agent scope.
/// `id` is the agent's uuid, `kind` is empty when the agent has no
/// kind (UI shows "(any kind)").
pub struct AgentOption {
    pub id: String,
    pub name: String,
    pub class: String,
    pub kind: String,
}

#[derive(Template)]
#[template(path = "scheduled_prompt_form.html")]
pub struct ScheduledPromptFormTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub prompt: Option<crate::entity::scheduled_prompt::Model>,
    pub form_action: String,
    pub classes: Vec<String>,
    pub kinds: Vec<String>,
    pub agents: Vec<AgentOption>,
    pub scope: String,
    /// Pre-rendered strings so Askama doesn't have to deref or compare
    /// `Option<String>` values against the option lists. Empty when
    /// the row is agent-scoped.
    pub target_class: String,
    pub target_kind: String,
    /// Pre-selected agent for agent-scope rows. `None` for team-scope
    /// rows and for new schedules.
    pub agent_id: Option<uuid::Uuid>,
    pub name: String,
    pub prompt_text: String,
    pub interval_seconds: i64,
    pub enabled: bool,
    pub ignore_inbox_state: bool,
    pub ignore_pending_forgejo_work: bool,
    pub weekly_safety_buffer_pct: i32,
    pub session_safety_buffer_pct: i32,
    pub context_clear_threshold_tokens: Option<i64>,
    pub runs: Vec<ScheduledPromptRunRow>,
}

/// Per-run view-model for the run-history table. Askama can't call
/// free functions, so the badge class is precomputed by the handler.
pub struct ScheduledPromptRunRow {
    pub run: crate::entity::scheduled_prompt_run::Model,
    pub outcome_badge: String,
}

/// Human-readable interval formatter for the scheduled prompts index.
/// e.g. `90` → `"1m 30s"`, `3600` → `"1h"`, `86400` → `"1d"`,
/// `12345` → `"3h 25m 45s"`. Always omits zero components.
pub fn format_interval(seconds: i64) -> String {
    if seconds <= 0 {
        return "0s".into();
    }
    let mut s = seconds;
    let days = s / 86400;
    s %= 86400;
    let hours = s / 3600;
    s %= 3600;
    let minutes = s / 60;
    let secs = s % 60;
    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if secs > 0 {
        parts.push(format!("{secs}s"));
    }
    parts.join(" ")
}

/// Bootstrap badge class for a run `outcome` string. Centralized so
/// the list page, the form-page history, and any future surfaces
/// agree on the color coding.
pub fn outcome_badge(outcome: &str) -> &'static str {
    match outcome {
        "completed" => "bg-success",
        "completed_error" => "bg-warning text-dark",
        "needs_input_canceled" => "bg-warning text-dark",
        "warren_restart" => "bg-warning text-dark",
        "rabbit_offline" => "bg-secondary",
        "failed" => "bg-danger",
        "fired" => "bg-secondary",
        "skipped_offline" | "skipped_no_idle" | "skipped_no_inbox" | "skipped_unsafe_scrape" => {
            "bg-info text-dark"
        }
        "skipped_no_matching_agent" | "skipped_no_idle_agent" => "bg-info text-dark",
        "skipped_weekly_budget" | "skipped_session_budget" => "bg-info text-dark",
        other if other.starts_with("skipped_") => "bg-info text-dark",
        _ => "bg-secondary",
    }
}
