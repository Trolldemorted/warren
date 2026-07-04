use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub warren_url: String,
    pub agent_token: String,
    pub workdir: String,
    pub claude_bin: String,
    pub claude_args: Vec<String>,
    #[allow(dead_code)]
    pub model: Option<String>,
    pub term_cols: u16,
    pub term_rows: u16,
    pub replay_bytes: usize,
    #[allow(dead_code)]
    pub observer_port: u16,
    pub health_port: u16,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let warren_url =
            env::var("WARREN_URL").map_err(|_| anyhow::anyhow!("WARREN_URL must be set"))?;
        let agent_token =
            env::var("AGENT_TOKEN").map_err(|_| anyhow::anyhow!("AGENT_TOKEN must be set"))?;
        let workdir = env::var("WORKDIR").unwrap_or_else(|_| "/work".to_string());
        let claude_bin = env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        let claude_args = env::var("CLAUDE_ARGS")
            .unwrap_or_else(|_| "--dangerously-skip-permissions".to_string())
            .split_whitespace()
            .map(String::from)
            .collect();
        let model = env::var("MODEL").ok().filter(|s| !s.is_empty());
        let term_cols = env::var("TERM_COLS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let term_rows = env::var("TERM_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40);
        let replay_bytes = env::var("REPLAY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(262_144);
        let observer_port = env::var("OBSERVER_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7777);
        let health_port = env::var("HEALTH_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        Ok(Self {
            warren_url,
            agent_token,
            workdir,
            claude_bin,
            claude_args,
            model,
            term_cols,
            term_rows,
            replay_bytes,
            observer_port,
            health_port,
        })
    }
}
