use std::env;
use std::path::PathBuf;

/// TUI grid size bounds. Anything outside this range is silently
/// clamped; the JS template and the rabbit PTY both reject extremes,
/// so we round-trip the same cap everywhere.
pub const TUI_COLS_MIN: u16 = 20;
pub const TUI_COLS_MAX: u16 = 500;
pub const TUI_ROWS_MIN: u16 = 5;
pub const TUI_ROWS_MAX: u16 = 200;

/// Clamp a cols/rows pair parsed from `TUI_WIDTH`/`TUI_HEIGHT`. Out-of-range
/// values fall back to the default rather than rejecting startup — the
/// grid is a presentation concern, not a correctness concern.
fn clamp_tui(raw: Option<&str>, default: u16, lo: u16, hi: u16) -> u16 {
    raw.and_then(|v| v.parse::<u16>().ok())
        .map(|n| n.clamp(lo, hi))
        .unwrap_or(default)
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub admin_psk: String,
    pub session_ttl_hours: i64,
    pub static_dir: PathBuf,
    pub docs_dir: PathBuf,
    /// Static terminal grid size. Read from `TUI_WIDTH` / `TUI_HEIGHT`
    /// at startup, defaults to 120 × 40. Warren is the source of truth:
    /// the value threads into the xterm.js template (so every browser
    /// renders the same grid) and into a `TuiConfig` envelope sent over
    /// the rabbit→warren WS so the spawned PTY winsize matches.
    pub tui_cols: u16,
    pub tui_rows: u16,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let admin_psk = env::var("WARREN_ADMIN_PSK")
            .map_err(|_| anyhow::anyhow!("WARREN_ADMIN_PSK must be set"))?;
        if admin_psk.is_empty() {
            anyhow::bail!("WARREN_ADMIN_PSK must not be empty");
        }
        let static_dir = env::var("WARREN_STATIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));
        let docs_dir = env::var("WARREN_DOCS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs"));
        Ok(Self {
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| {
                "host=localhost user=warren password=warren dbname=warren".into()
            }),
            admin_psk,
            session_ttl_hours: env::var("SESSION_TTL_HOURS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(168),
            static_dir,
            docs_dir,
            tui_cols: clamp_tui(
                env::var("TUI_WIDTH").ok().as_deref(),
                120,
                TUI_COLS_MIN,
                TUI_COLS_MAX,
            ),
            tui_rows: clamp_tui(
                env::var("TUI_HEIGHT").ok().as_deref(),
                40,
                TUI_ROWS_MIN,
                TUI_ROWS_MAX,
            ),
        })
    }

    /// Load only what's needed to run `atlas migrate apply`: just the
    /// database URL. The admin PSK is irrelevant for migrations.
    pub fn from_env_for_migrations() -> anyhow::Result<Self> {
        let database_url = env::var("DATABASE_URL")
            .unwrap_or_else(|_| "host=localhost user=warren password=warren dbname=warren".into());
        let static_dir = env::var("WARREN_STATIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"));
        let docs_dir = env::var("WARREN_DOCS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs"));
        Ok(Self {
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            database_url,
            admin_psk: String::new(),
            session_ttl_hours: 168,
            static_dir,
            docs_dir,
            // Migrations don't render templates, but the field is required
            // by the struct. Use defaults — they are never observed.
            tui_cols: 120,
            tui_rows: 40,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a TUI_WIDTH value the same way `from_env` does, but without
    /// pulling in the rest of the env (DATABASE_URL, WARREN_ADMIN_PSK).
    /// Used by the unit tests for the env-var path.
    fn parse_tui_width() -> u16 {
        clamp_tui(
            env::var("TUI_WIDTH").ok().as_deref(),
            120,
            TUI_COLS_MIN,
            TUI_COLS_MAX,
        )
    }

    fn parse_tui_height() -> u16 {
        clamp_tui(
            env::var("TUI_HEIGHT").ok().as_deref(),
            40,
            TUI_ROWS_MIN,
            TUI_ROWS_MAX,
        )
    }

    #[test]
    fn clamp_tui_default_when_unset() {
        // SAFETY: this test mutates process env. We serialize via a
        // single test (the others use unscoped vars) and accept the
        // race — `cargo test` runs tests in parallel and the env is
        // shared. We avoid `set_var` here and only check that the
        // defaults land at 120/40 when neither var is set. Other
        // tests assert the clamping bounds.
        let saved_w = env::var("TUI_WIDTH").ok();
        let saved_h = env::var("TUI_HEIGHT").ok();
        env::remove_var("TUI_WIDTH");
        env::remove_var("TUI_HEIGHT");
        assert_eq!(parse_tui_width(), 120);
        assert_eq!(parse_tui_height(), 40);
        if let Some(v) = saved_w {
            env::set_var("TUI_WIDTH", v);
        }
        if let Some(v) = saved_h {
            env::set_var("TUI_HEIGHT", v);
        }
    }

    #[test]
    fn clamp_tui_bounds_cols() {
        let cases: &[(&str, u16)] = &[
            ("0", TUI_COLS_MIN),
            ("19", TUI_COLS_MIN),
            ("20", 20),
            ("200", 200),
            ("500", TUI_COLS_MAX),
            ("501", TUI_COLS_MAX),
            ("65535", TUI_COLS_MAX),
        ];
        for (input, expected) in cases {
            assert_eq!(
                clamp_tui(Some(input), 120, TUI_COLS_MIN, TUI_COLS_MAX),
                *expected,
                "input {input} should clamp to {expected}"
            );
        }
    }

    #[test]
    fn clamp_tui_bounds_rows() {
        let cases: &[(&str, u16)] = &[
            ("0", TUI_ROWS_MIN),
            ("4", TUI_ROWS_MIN),
            ("5", 5),
            ("40", 40),
            ("200", TUI_ROWS_MAX),
            ("999", TUI_ROWS_MAX),
        ];
        for (input, expected) in cases {
            assert_eq!(
                clamp_tui(Some(input), 40, TUI_ROWS_MIN, TUI_ROWS_MAX),
                *expected,
                "input {input} should clamp to {expected}"
            );
        }
    }

    #[test]
    fn clamp_tui_garbage_falls_back_to_default() {
        assert_eq!(
            clamp_tui(Some("not a number"), 120, TUI_COLS_MIN, TUI_COLS_MAX),
            120
        );
        assert_eq!(clamp_tui(Some(""), 40, TUI_ROWS_MIN, TUI_ROWS_MAX), 40);
        assert_eq!(clamp_tui(Some("-1"), 120, TUI_COLS_MIN, TUI_COLS_MAX), 120);
    }
}
