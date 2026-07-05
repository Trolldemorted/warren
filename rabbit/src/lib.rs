//! `rabbit` — the per-agent supervisor that wraps a real `claude` PTY and
//! bridges it to warren over a WebSocket link.
//!
//! This library exists so the supervisor's internals (`pty`, `input`,
//! `observer::*`, `wire`, `respawn`, …) are reachable from integration tests
//! under `tests/` and from downstream crates that want to embed rabbit's
//! PTY/observer pieces. The `rabbit` binary (`src/main.rs`) is a thin wrapper
//! around [`run`]; the `rabbit-hook` binary is independent.

pub mod config;
pub mod health;
pub mod hooks_install;
pub mod http_server;
pub mod input;
pub mod link;
pub mod meta_ring;
pub mod observer;
pub mod pty;
pub mod recorder;
pub mod respawn;
pub mod shell;
pub mod supervisor;
pub mod trust;
pub mod vt;
pub mod wire;

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
                // HookShim is now its own binary at src/bin/rabbit-hook.rs.
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
