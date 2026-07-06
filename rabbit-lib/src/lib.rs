//! `rabbit-lib` — the per-agent Claude supervisor plus the matching
//! server-side runtime that warren embeds.
//!
//! Two halves glued together by a single `wire` module:
//!
//! * The **supervisor half** (`pty`, `vt`, `input`, `trust`, `respawn`,
//!   `shell`, `recorder`, `observer`, `meta_ring`, `link`, `supervisor`)
//!   spawns a real `claude` PTY, parses its terminal output, observes its
//!   lifecycle via Claude's hook protocol, optionally records asciicast,
//!   and bridges everything to a single WebSocket link.
//! * The **server half** (`server`) accepts the supervisor's WS, fans
//!   term-bytes and meta-envelopes out to multiple browser subscribers
//!   per agent, and persists the event stream via the `SessionStore` trait
//!   (warren plugs in a `SeaOrmSessionStore`).
//!
//! The `rabbit` and `rabbit-hook` binaries are thin wrappers around
//! [`run`] and the hook-shim loop respectively, kept as separate bins
//! so the on-disk artifacts don't change.

// Modules that are part of the library's external API. `server` is the
// only consumer-facing surface (warren embeds it); the others are pub so
// the crate's `tests/*.rs` integration suites can reach them — those
// tests are external to the library crate and so can't see `pub(crate)`
// items.
pub mod input;
pub mod link;
pub mod meta_ring;
pub mod observer;
pub mod pty;
pub mod server;
pub mod supervisor;
pub mod trust;
pub mod vt;
pub mod wire;

// Crate-internal. None of these are reached by external consumers
// (warren, integration tests, or the `rabbit` binary beyond `run()`).
pub(crate) mod config;
pub(crate) mod health;
pub(crate) mod hooks_install;
pub(crate) mod http_server;
pub(crate) mod recorder;
pub(crate) mod respawn;
pub(crate) mod shell;

use anyhow::Result;
use std::path::Path;

/// Synchronous entry point for the `rabbit` supervisor binary.
///
/// Installs a backtrace-capturing panic hook, initializes logging, detects the
/// run mode from `argv`, builds the Tokio runtime, and drives the supervisor to
/// completion. On error it logs, prints to stderr, and exits the process with a
/// non-zero code (so `main` stays a one-liner).
pub fn run() -> Result<()> {
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("panic: {info}\n{bt}");
        log::error!("panic: {info}");
        log::error!("backtrace:\n{bt}");
    }));
    if let Err(e) = simple_logger::init_with_env() {
        eprintln!("error: failed to initialize logger: {e:?}");
    }

    let argv0 = std::env::args().next().unwrap_or_default();
    let mode = detect_mode(&argv0, std::env::args().nth(1).as_deref());

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
        match mode {
            Mode::Supervisor => {
                let cfg = config::Config::from_env()?;
                supervisor::run(cfg).await
            }
            Mode::HookShim => {
                // HookShim is its own binary at src/bin/rabbit-hook.rs.
                // If you ever invoke it through the rabbit binary, that's a
                // bug — bail with a clear error.
                Err(anyhow::anyhow!(
                    "rabbit-hook must be invoked as the `rabbit-hook` binary, not via {}",
                    argv0
                ))
            }
        }
    });

    if let Err(e) = result {
        log::error!("rabbit failed: {e:?}");
        eprintln!("error: rabbit failed: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}

enum Mode {
    Supervisor,
    HookShim,
}

fn detect_mode(argv0: &str, argv1: Option<&str>) -> Mode {
    let base = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if base == "rabbit-hook" || argv1 == Some("hook-shim") {
        Mode::HookShim
    } else {
        Mode::Supervisor
    }
}
