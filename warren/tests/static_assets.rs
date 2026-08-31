//! Vendored static asset existence test.
//!
//! The agent pages (`/agent/:id/claude`, `/agent/:id/shell`) link to a
//! fixed set of vendored assets in `static/vendor/`. The templates
//! assert the *paths* in `agent_page_smoke::templates_reference_required_assets`,
//! but nothing checked that the files actually live on disk where the
//! `tower_http::services::ServeDir` will look for them. A regression
//! that drops one of these from the repo would slip past every test:
//! `xterm.js` 404 would throw inside `Terminal()` and the page would
//! render an empty `#term` div — exactly the "fully-black terminal
//! pane" symptom in desktop Chrome, but only on a clean checkout.
//!
//! Cheap, always-on, no DB, no HTTP. Catches the case where someone
//! runs `git clean` too aggressively or a refactor moves `static/`
//! without updating the vendored files.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn exists(rel: &str) -> bool {
    repo_root().join(rel).is_file()
}

#[test]
fn vendored_xterm_assets_present() {
    let assets = [
        "static/vendor/xterm/xterm.js",
        "static/vendor/xterm/xterm.css",
        "static/vendor/xterm/xterm-addon-fit.js",
        "static/vendor/xterm/NOTICE",
    ];
    for rel in assets {
        assert!(
            exists(rel),
            "vendored asset missing from repo: {rel} — the agent page will \
             reference a 404 and xterm.js will throw at `new Terminal(...)`"
        );
    }
}

#[test]
fn vendored_bootstrap_present() {
    assert!(
        exists("static/vendor/bootstrap.min.css"),
        "vendored bootstrap missing from repo: static/vendor/bootstrap.min.css \
         — base.html loads it on every page"
    );
}

/// Catch the "stale binary after a template edit" regression. Askama
/// compiles templates into the binary (see CLAUDE.md "Schema work" +
/// the Askama note in the same file), so an updated template is only
/// reflected at runtime once the binary is rebuilt. This test asserts
/// that the debug binary on disk is newer than every `.html` file
/// under `templates/`. When the check fails, the developer either
/// hasn't rebuilt since the last template edit, or the runtime they
/// are hitting is serving the older binary (the agent that flagged
/// this regression was looking at `target/release/warren` from Jul 8
/// while the templates had been edited more recently — Askama didn't
/// get a chance to rebake).
///
/// Only `target/debug/warren` is checked. `target/release/warren` is
/// produced by the Dockerfile for production and is not part of the
/// dev loop; if it exists locally, it's almost certainly stale and
/// should not be used for local testing. We deliberately don't fail
/// on its absence or its age — the release build is a CI/Docker
/// concern.
#[test]
fn debug_binary_newer_than_templates() {
    use std::time::SystemTime;

    let bin = repo_root().join("target").join("debug").join("warren");
    if !bin.is_file() {
        // Nothing built yet — `cargo build` will be a no-op against
        // already-built artifacts, but the dev who hasn't built yet
        // will build on the next `cargo test` invocation anyway.
        // Skip silently rather than failing CI on a fresh checkout.
        eprintln!(
            "skip: {} not built yet — run `cargo build -p warren --bin warren`",
            bin.display()
        );
        return;
    }
    let bin_mtime = bin
        .metadata()
        .and_then(|m| m.modified())
        .expect("read binary mtime");
    let templates_dir = repo_root().join("templates");
    let mut newest_template: Option<(PathBuf, SystemTime)> = None;
    let entries = std::fs::read_dir(&templates_dir).expect("read templates dir");
    for entry in entries {
        let entry = entry.expect("read template dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .expect("read template mtime");
        if newest_template
            .as_ref()
            .map(|(_, t)| mtime > *t)
            .unwrap_or(true)
        {
            newest_template = Some((path, mtime));
        }
    }
    let (newest_path, newest_mtime) = newest_template.expect("at least one .html under templates/");
    if bin_mtime < newest_mtime {
        panic!(
            "{} was modified at {:?} but {} was last built at {:?}. \
             Askama bakes templates into the binary — run \
             `cargo build -p warren --bin warren` (or \
             `cargo build --workspace`) and restart the running warren \
             process before testing again.",
            newest_path.display(),
            newest_mtime,
            bin.display(),
            bin_mtime,
        );
    }
}
