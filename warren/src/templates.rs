use askama::Template;
use serde::Serialize;

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
pub struct CommsTemplate {
    pub title: Option<&'static str>,
    pub nav: Option<&'static str>,
    pub flash: Option<Flash>,
    pub requests: Vec<crate::entity::request::Model>,
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
