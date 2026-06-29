use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub admin_psk: String,
    pub session_ttl_hours: i64,
    pub static_dir: PathBuf,
    pub docs_dir: PathBuf,
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
                .unwrap_or(24),
            static_dir,
            docs_dir,
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
            session_ttl_hours: 24,
            static_dir,
            docs_dir,
        })
    }
}
