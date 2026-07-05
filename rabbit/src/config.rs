use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub warren_url: String,
    pub agent_token: String,
    pub workdir: String,
    pub claude_bin: String,
    pub claude_args: Vec<String>,
    pub model: Option<String>,
    pub term_cols: u16,
    pub term_rows: u16,
    pub replay_bytes: usize,
    #[allow(dead_code)]
    pub observer_port: u16,
    pub health_port: u16,
    pub shutdown_grace_ms: u64,
    pub crash_window_secs: u64,
    pub crash_threshold: usize,
    pub hook_bin: Option<PathBuf>,
    /// Upper bound on bytes of unacked meta events buffered for replay on
    /// WS reconnect. Default 256 KiB. Long disconnects lose the oldest
    /// buffered events when this is exceeded; the bounded terminal replay
    /// buffer (§A.6) preserves the screen state regardless.
    pub meta_ring_bytes: usize,
    /// Auto-accept claude's first-run "do you trust this folder?" dialog by
    /// injecting Enter when it's detected in the PTY output. Defaults on so an
    /// unattended pod on a fresh PVC mount (§A.7) doesn't hang on first boot.
    /// Set `AUTO_TRUST=0` to disable (e.g. if the workdir is pre-trusted).
    pub auto_trust: bool,
    /// §D Milestone 5: spawn a second PTY (`bash -i`) on the same rabbit
    /// and expose it at `/agent/:id/shell` on the warren side. Disabled by
    /// default — most production agents don't need a debug shell.
    pub enable_shell: bool,
    /// Binary to run for the shell PTY. Defaults to `/bin/bash -i`.
    pub shell_bin: String,
    pub shell_args: Vec<String>,
    /// §D Milestone 5: record every claude session to asciicast v2 `.cast`
    /// files in `asciicast_dir`. Drives the `/agent/:id/claude/history`
    /// page on the warren side. Disabled by default — the recording is
    /// off the critical path and only useful when an operator wants to
    /// inspect past sessions.
    pub enable_asciicast: bool,
    /// Where to write `.cast` files. Defaults to `<workdir>/.claude/casts`.
    /// Created on first start if missing.
    pub asciicast_dir: PathBuf,
    /// Per-session size cap in bytes. When a single `<id>.cast` would
    /// exceed this, it rotates to `<id>.cast.0`, `.cast.1`, … and the
    /// oldest segment is evicted. Defaults to 10 MiB (TODO §D retention
    /// policy).
    pub asciicast_bytes_cap: u64,
    /// TCP port for the recorder HTTP server (serves `.cast` files to
    /// warren). Distinct from `health_port` so k8s probes and operator
    /// fetches don't share a port.
    pub recorder_http_port: u16,
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
        let shutdown_grace_ms = env::var("SHUTDOWN_GRACE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500);
        let crash_window_secs = env::var("CRASH_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let crash_threshold = env::var("CRASH_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let hook_bin = env::var("RABBIT_HOOK_BIN").ok().map(PathBuf::from);
        let meta_ring_bytes = env::var("META_RING_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(262_144);
        let auto_trust = env::var("AUTO_TRUST")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        let enable_shell = env::var("ENABLE_SHELL")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(false);
        let shell_bin = env::var("SHELL_BIN").unwrap_or_else(|_| "/bin/bash".to_string());
        let shell_args = env::var("SHELL_ARGS")
            .unwrap_or_else(|_| "-i".to_string())
            .split_whitespace()
            .map(String::from)
            .collect();
        let enable_asciicast = env::var("ENABLE_ASCIICAST")
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(false);
        let asciicast_dir = env::var("ASCIICAST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&workdir).join(".claude").join("casts"));
        let asciicast_bytes_cap = env::var("ASCIICAST_BYTES_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024);
        let recorder_http_port = env::var("RECORDER_HTTP_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7790);
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
            shutdown_grace_ms,
            crash_window_secs,
            crash_threshold,
            hook_bin,
            meta_ring_bytes,
            auto_trust,
            enable_shell,
            shell_bin,
            shell_args,
            enable_asciicast,
            asciicast_dir,
            asciicast_bytes_cap,
            recorder_http_port,
        })
    }
}
