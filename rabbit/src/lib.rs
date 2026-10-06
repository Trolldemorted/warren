//! `rabbit` — the per-agent Claude supervisor. Spawns `claude` in a
//! PTY, observes its lifecycle via Claude's hook protocol, and bridges
//! everything to a warren broker over a single WebSocket.
//!
//! The wire-protocol types (`Envelope`, `EnvelopeBody`, `TermFrame`,
//! `AgentState`, etc.) are owned by [`rabbit_lib::wire`] so the server
//! half in `rabbit_lib::server` can deserialize what this crate produces.
//! They are re-exported here as `rabbit::wire` for ergonomic call sites
//! inside this crate's modules.

pub mod config;
pub mod dispatch;
pub mod health;
pub mod hooks_install;
pub mod input;
pub mod link;
pub mod meta_ring;
pub mod observer;
pub mod pty;
pub mod pty_writer;
pub mod respawn;
pub mod shell;
pub mod supervisor;
pub mod trust;
pub mod vt;

pub use rabbit_lib::wire;

/// Synchronous entry point for the `rabbit` supervisor binary.
///
/// Installs a backtrace-capturing panic hook, initializes logging, builds the
/// Tokio runtime, and drives the supervisor to completion. On error it logs,
/// prints to stderr, and exits the process with a non-zero code (so `main`
/// stays a one-liner).
pub fn run() -> anyhow::Result<()> {
    /// Initialise logging with a sane default.
    ///
    /// `simple_logger::init_with_env()` defaults to TRACE when `RUST_LOG` is
    /// unset, which turns on `tungstenite`'s per-frame protocol logging — every
    /// WebSocket message printed as a byte array, continuously. On a busy agent
    /// that is megabytes a minute and buries the log that matters.
    ///
    /// Default to `info` for our own crates and cap the chatty transport
    /// dependencies at `warn`. An explicit `RUST_LOG` still wins, so debugging
    /// the wire is a deliberate opt-in.
    fn init_logging() {
        if std::env::var_os("RUST_LOG").is_some() {
            let _ = simple_logger::init_with_env();
            return;
        }
        let _ = simple_logger::SimpleLogger::new()
            .with_level(log::LevelFilter::Info)
            .with_module_level("tungstenite", log::LevelFilter::Warn)
            .with_module_level("hyper", log::LevelFilter::Warn)
            .with_module_level("h2", log::LevelFilter::Warn)
            .with_module_level("reqwest", log::LevelFilter::Warn)
            .with_module_level("rustls", log::LevelFilter::Warn)
            .init();
    }

    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("panic: {info}\n{bt}");
        log::error!("panic: {info}");
        log::error!("backtrace:\n{bt}");
    }));
    init_logging();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build tokio runtime: {e:?}");
            std::process::exit(1);
        }
    };

    let result = runtime.block_on(async move {
        let cfg = config::Config::from_env()?;
        supervisor::run(cfg).await
    });

    if let Err(e) = result {
        log::error!("rabbit failed: {e:?}");
        eprintln!("error: rabbit failed: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}
