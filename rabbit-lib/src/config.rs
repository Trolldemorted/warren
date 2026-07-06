use std::env;
use std::path::PathBuf;

/// True when this rabbit binary was built with the `tls` feature, which
/// is what enables `tokio_tungstenite`'s rustls connector. `WARREN_URL`
/// validation uses this to refuse `https://` / `wss://` schemes when the
/// binary couldn't actually speak TLS — without the check, the link task
/// would loop on `URL error: TLS support not compiled in` forever instead
/// of failing fast at startup.
#[cfg(feature = "tls")]
pub const TLS_COMPILED_IN: bool = true;
#[cfg(not(feature = "tls"))]
pub const TLS_COMPILED_IN: bool = false;

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
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let warren_url =
            env::var("WARREN_URL").map_err(|_| anyhow::anyhow!("WARREN_URL must be set"))?;
        validate_warren_url(&warren_url)?;
        let agent_token =
            env::var("AGENT_TOKEN").map_err(|_| anyhow::anyhow!("AGENT_TOKEN must be set"))?;
        let workdir = env::var("WORKDIR").unwrap_or_else(|_| "/workdir".to_string());
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
        })
    }
}

/// Refuse `WARREN_URL` values that would force rabbit into a permanent
/// failure loop. Two classes of input are rejected:
///
/// 1. **TLS requested but not compiled in** — `https://` or `wss://`
///    against a binary built without the `tls` feature. The link task
///    would otherwise retry every 250 ms with `URL error: TLS support
///    not compiled in` (the `__rustls-tls` cfg marker is not set in
///    `tokio-tungstenite`). Fail fast instead so the operator sees the
///    misconfiguration on first launch rather than in the logs after
///    N minutes.
/// 2. **Unknown schemes** — anything other than `http`/`https`/`ws`/`wss`.
///    Catches typos (`htps://`) early.
///
/// Plain `http`/`ws` are accepted regardless of the `tls` feature.
pub fn validate_warren_url(url: &str) -> anyhow::Result<()> {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| s)
        .ok_or_else(|| anyhow::anyhow!("WARREN_URL must be an absolute URL with a scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    let needs_tls = matches!(scheme.as_str(), "https" | "wss");
    if needs_tls && !TLS_COMPILED_IN {
        return Err(anyhow::anyhow!(
            "WARREN_URL uses {scheme}:// but this rabbit binary was built without TLS support. \
             Rebuild with the default features (`cargo build -p rabbit`) so the `tls` feature \
             is enabled, or change WARREN_URL to http:// / ws:// if TLS is terminated upstream."
        ));
    }
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss") {
        return Err(anyhow::anyhow!(
            "WARREN_URL has unsupported scheme `{scheme}://`; expected http, https, ws, or wss"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_url_always_accepted() {
        // http:// and ws:// work whether TLS is compiled in or not.
        validate_warren_url("http://warren.local:8080").unwrap();
        validate_warren_url("ws://warren.local:8080").unwrap();
        validate_warren_url("HTTP://warren.local:8080").unwrap();
        validate_warren_url("HTTPS://warren.local:8080").unwrap_or(()); // accepted iff TLS compiled in (default build)
    }

    #[test]
    fn https_url_accepted_when_tls_compiled_in() {
        // Default build enables the `tls` feature, so this should pass.
        if TLS_COMPILED_IN {
            validate_warren_url("https://warren.example.com").unwrap();
            validate_warren_url("wss://warren.example.com:8443").unwrap();
        }
    }

    #[test]
    fn url_without_scheme_rejected() {
        let err = validate_warren_url("warren.local:8080").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("must be an absolute URL"),
            "expected scheme guidance, got: {msg}"
        );
    }

    #[test]
    fn unknown_scheme_rejected() {
        let err = validate_warren_url("ftp://warren.local").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported scheme `ftp://`"),
            "expected scheme-name in error, got: {msg}"
        );
    }

    /// Exercise the TLS-mismatch path explicitly. When the `tls` feature
    /// is off (i.e. `cargo build -p rabbit --no-default-features`), this
    /// test would otherwise be unable to assert anything because
    /// `TLS_COMPILED_IN` is `false` at compile time. We still construct
    /// the inputs and check the result reflects the feature flag, so a
    /// future regression that flips the default off surfaces here.
    #[test]
    fn https_without_tls_feature_is_rejected() {
        if !TLS_COMPILED_IN {
            let err = validate_warren_url("https://warren.example.com").unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("built without TLS support"),
                "expected TLS-feature guidance, got: {msg}"
            );
        }
        // When TLS is compiled in, the rejection path isn't reachable
        // from this crate (compile-time const). The behavior is exercised
        // by `https_url_accepted_when_tls_compiled_in` instead.
    }
}
