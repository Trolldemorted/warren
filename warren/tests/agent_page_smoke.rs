//! HTTP smoke test for the agent pages.
//!
//! Two layers:
//!
//! 1. `templates_reference_required_assets` — cheap, always-on. Reads
//!    the Askama templates from disk and asserts each page that mounts
//!    a terminal still references every required static asset and
//!    runtime knob. Catches the easy regression class where a template
//!    edit drops a `<script>`/`<link>` tag, removes the `#term` mount,
//!    or accidentally overwrites the layout fix that turned the
//!    terminal pane from fully-black back to its intended size (the
//!    `grid-template-rows: minmax(0, 1fr)` declaration the mobile
//!    media query does not cover).
//!
//! 2. `http_smoke_*` (gated `#[ignore]`) — heavy, end-to-end. Spins up
//!    a scratch Postgres on the test cluster, applies migrations via
//!    `atlas`, spawns `warren server` as a subprocess, logs in via
//!    the admin PSK, creates an agent, and GETs the page over real
//!    HTTP. Mirrors `tests/mobile-layout/run.sh` step 3/4 minus
//!    chromium. Skips gracefully when postgres / atlas / warren isn't
//!    available, matching the existing schema_drift pattern.
//!
//! Both layers are needed. The static test runs in the standard cargo
//! loop (no `#[ignore]`) so any template regression shows up in
//! `cargo test -p warren` immediately; the HTTP test runs only on
//! `--ignored` so contributors without postgres aren't broken.

use std::path::PathBuf;
use std::process::Command;

fn template_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("templates")
        .join(rel)
}

fn read_template(rel: &str) -> String {
    let p = template_path(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read template {}: {e}", p.display()))
}

/// Every terminal-mounting template must reference these runtime hooks.
/// Adding a new template that mounts a terminal? Mirror these
/// assertions in the new template's static check too.
fn assert_terminal_template_contract(name: &str, body: &str) {
    // The xterm.js mount. Without it `Terminal` throws and the page
    // renders an empty div.
    assert!(
        body.contains(r#"<div id="term">"#),
        "[{name}] missing `<div id=\"term\">` — xterm.js has nowhere to mount"
    );
    // The vendored xterm bundle. Four separate artifacts because the
    // template loads the CSS + UMD bundle + fit addon via three
    // different tags. Drop any of them and the canvas goes blank.
    assert!(
        body.contains(r#"href="/static/vendor/xterm/xterm.css""#),
        "[{name}] missing xterm.css link tag"
    );
    assert!(
        body.contains(r#"src="/static/vendor/xterm/xterm.js""#),
        "[{name}] missing xterm.js script tag"
    );
    assert!(
        body.contains(r#"src="/static/vendor/xterm/xterm-addon-fit.js""#),
        "[{name}] missing xterm-addon-fit.js script tag"
    );
    // JS constants + constructor — guards against accidental
    // removal of the entire inline `<script>` block.
    assert!(
        body.contains("TERM_COLS"),
        "[{name}] missing TERM_COLS — FitAddon has no size to apply"
    );
    assert!(
        body.contains("new Terminal("),
        "[{name}] missing `new Terminal(` — the inline JS was probably truncated"
    );
    // The desktop layout fix. Without
    // `grid-template-rows: minmax(0, 1fr)` the grid row auto-sizes
    // to the aside's content height on desktop and the terminal pane
    // renders as fully black (`#0d1117` background covering ~95
    // rows of empty canvas). The mobile breakpoint overrides the
    // grid display entirely, so the row declaration only matters on
    // desktop — but the rule must exist on every terminal-mounting
    // template that uses the grid layout.
    assert!(
        body.contains("grid-template-rows: minmax(0, 1fr)"),
        "[{name}] missing `grid-template-rows: minmax(0, 1fr)` — the desktop \
         terminal pane will render as fully black (the row auto-sizes to \
         the aside's intrinsic content height instead of filling the grid)"
    );
}

#[test]
fn templates_reference_required_assets() {
    assert_terminal_template_contract("agent_claude", &read_template("agent_claude.html"));
    assert_terminal_template_contract("agent_shell", &read_template("agent_shell.html"));
}

#[test]
fn templates_expose_term_for_layout_test() {
    // The mobile-layout probe reads `window.term.buffer.active` to
    // detect "xterm was sized but never painted anything". Forgetting
    // the `window.term = term` line on the claude template would
    // silently downgrade the suite to dimension-only. The shell
    // template doesn't expose it (no FitAddon, no probe reads
    // through `window.term`), so don't pin that side here.
    let body = read_template("agent_claude.html");
    assert!(
        body.contains("window.term = term"),
        "agent_claude.html must expose `window.term` for the mobile-layout \
         content probe — without it the dimension-only assertions will silently \
         regress the 'fully-black terminal pane' class of bug"
    );
}

/// Catch the "lexical TDZ throw aborts the script before `connectWs()`
/// runs" regression on the claude page. `refitWhenReady(20)` is invoked
/// at top level and calls `refit()` → `maybeSendResize()` on the first
/// valid frame. `maybeSendResize()` reads the module-scoped `ws`/`connected`
/// bindings. If the `let ws = null;` / `let connected = false;`
/// declarations sit BELOW the `refitWhenReady(20)` call site, the
/// script throws `ReferenceError: can't access lexical declaration
/// 'ws' before initialization` from inside the rAF tick, the whole
/// `<script>` aborts, and `connectWs()` is never reached — the browser
/// then opens zero WebSockets and the terminal pane stays fully
/// black. This test pins the ordering so that exact failure mode can't
/// silently regress.
///
/// The shell template doesn't have this TDZ hazard (no `refitWhenReady`
/// loop, no `maybeSendResize`), so it's deliberately not pinned here.
#[test]
fn agent_claude_template_declares_ws_state_before_refit_when_ready() {
    let body = read_template("agent_claude.html");
    // Match the call site (`refitWhenReady(20);` with the trailing
    // semicolon) rather than the bare identifier — the explanatory
    // comment above the `let ws = null;` block also mentions
    // `refitWhenReady(20)`, which would otherwise match first and
    // invert the ordering check.
    let refit_pos = body
        .find("refitWhenReady(20);")
        .expect("agent_claude.html no longer calls `refitWhenReady(20)`");
    for decl in [
        "let ws = null;",
        "let wsBackoff = 500;",
        "let connected = false;",
    ] {
        let count = body.matches(decl).count();
        assert_eq!(
            count, 1,
            "agent_claude.html must declare `{decl}` exactly once (found {count}). \
             Multiple declarations cause the second `let` to throw a SyntaxError, \
             and zero means `ws`/`wsBackoff`/`connected` are accessed before \
             initialization."
        );
        let pos = body.find(decl).unwrap();
        assert!(
            pos < refit_pos,
            "agent_claude.html declares `{decl}` at byte offset {pos}, which is \
             AFTER `refitWhenReady(20)` at byte offset {refit_pos}. \
             `refitWhenReady` -> `refit` -> `maybeSendResize` reads `ws`/`connected` \
             in the first rAF tick; if those `let` bindings are still in their \
             temporal dead zone the script throws and aborts before `connectWs()` \
             is ever called, leaving the browser with zero WebSockets."
        );
    }
}

/// Pin the auto-follow-output behavior on the claude page. After
/// Claude Code's "detail mode" toggle (Ctrl+O) the TUI re-emits
/// history above the prompt. xterm.js's default follow-output only
/// fires when the cursor is in the visible area; the cursor ends up
/// below the pre-toggle viewport, so the viewport stays parked at its
/// old scrollTop and the prompt (input bar) is left stranded mid-wrap
/// with stale scrollback below. The fix re-pins the viewport to the
/// bottom on every binary frame write, gated by a wheel/touch-scroll
/// listener that detects when the user has intentionally scrolled up
/// to read scrollback (in which case we leave the viewport alone so
/// the user isn't yanked back mid-keystroke).
///
/// This test pins the contract that future template edits must keep:
///   - a single `userScrolledAwayFromBottom` `let` is declared
///   - it is declared BEFORE the `watchUserScroll` IIFE that mutates it
///     (otherwise the script hits a TDZ throw at `term.open` time and
///     no xterm content ever reaches the user)
///   - `writeBinaryFrame` calls `scrollXtermToBottom()` in the
///     `pendingFrames === null` path (the only path that fires after
///     the first `screen_snapshot` apply, i.e. the steady state)
///   - the gate (`if (!userScrolledAwayFromBottom)`) sits between the
///     write and the scroll, so a user scrolled up to read scrollback
///     is not yanked back to the bottom on every frame.
#[test]
fn agent_claude_template_auto_follows_output_unless_user_scrolled_away() {
    let body = read_template("agent_claude.html");

    let flag_count = body
        .matches("let userScrolledAwayFromBottom = false;")
        .count();
    assert_eq!(
        flag_count, 1,
        "agent_claude.html must declare `let userScrolledAwayFromBottom = false;` \
         exactly once (found {flag_count}). Two declarations cause a SyntaxError; \
         zero means the auto-follow gate is missing entirely."
    );

    let decl_pos = body
        .find("let userScrolledAwayFromBottom = false;")
        .expect("flag declaration missing");

    // The IIFE that mutates the flag runs synchronously during script
    // init. If the `let` is below the IIFE call, the IIFE's body
    // executes while the binding is still in TDZ and the whole
    // `<script>` aborts — leaving the page with no scroll-follow
    // logic AND no xterm content.
    let iife_pos = body
        .find("function watchUserScroll()")
        .expect("watchUserScroll IIFE missing — auto-follow listener was removed");
    assert!(
        decl_pos < iife_pos,
        "`let userScrolledAwayFromBottom` at byte offset {decl_pos} is AFTER \
         the `watchUserScroll` IIFE at byte offset {iife_pos}. The IIFE runs \
         at script-init time and would hit a TDZ throw on `userScrolledAwayFromBottom`, \
         aborting the whole script before `connectWs()` is reached."
    );

    // The write path must re-pin to the bottom in the steady state.
    // Find the `writeBinaryFrame` body — it's a function declaration,
    // so any text occurrence with the right trailing `(` is unique.
    let write_pos = body
        .find("function writeBinaryFrame(seq, data) {")
        .expect("writeBinaryFrame function missing");
    let write_end = body[write_pos..]
        .find("}")
        .map(|p| write_pos + p)
        .expect("writeBinaryFrame body not closed");
    let write_body = &body[write_pos..write_end];
    assert!(
        write_body.contains("scrollXtermToBottom("),
        "writeBinaryFrame never calls `scrollXtermToBottom` — without it, \
         a Claude detail-mode toggle strands the prompt mid-wrap with \
         stale scrollback below."
    );
    assert!(
        write_body.contains("userScrolledAwayFromBottom"),
        "writeBinaryFrame never reads `userScrolledAwayFromBottom` — the \
         gate that protects users scrolled up in scrollback is missing, \
         so every frame yanks them back to the bottom."
    );
    // Order: write first, then check the flag, then scroll. The exact
    // `if (!userScrolledAwayFromBottom) scrollXtermToBottom();` line
    // is the contract — pin it so a future refactor doesn't reorder.
    assert!(
        write_body.contains("if (!userScrolledAwayFromBottom) scrollXtermToBottom();"),
        "writeBinaryFrame must call `scrollXtermToBottom()` *after* the \
         `term.write()` and *only* when the user hasn't scrolled away. \
         Look for the literal `if (!userScrolledAwayFromBottom) scrollXtermToBottom();`."
    );
}

// -- HTTP smoke test (gated). Mirrors tests/mobile-layout/run.sh step 3/4 --

fn has(cmd: &str) -> bool {
    // Also accept binaries that aren't on PATH but live at the
    // well-known dev paths the existing test pyramid relies on (the
    // schema_drift test similarly expects atlas at `/tmp/atlas`).
    Command::new(cmd).arg("--version").output().is_ok()
        || Command::new(format!("/tmp/{cmd}"))
            .arg("--version")
            .output()
            .is_ok()
}

fn warren_bin() -> Option<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_warren").map(PathBuf::from)
}

fn admin_url() -> String {
    std::env::var("WARREN_DRIFT_ADMIN_URL")
        .unwrap_or_else(|_| "postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable".into())
}

fn test_db_name() -> String {
    format!(
        "warren_smoke_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn psql_exec(url: &str, sql: &str) -> Result<String, String> {
    let out = Command::new("psql")
        .args([url, "-tAc", sql])
        .output()
        .map_err(|e| format!("psql spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "psql failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Boots warren against a scratch DB, returns (base_url, admin_cookie,
/// agent_id, child_handle) on success. Caller kills the child via
/// `cleanup_smoke`. The returned cookie is `warren_session=<value>` for
/// use in subsequent requests.
fn boot_warren_with_scratch_db(
    warren: &PathBuf,
    target_url: &str,
) -> Result<(String, String, String, std::process::Child), String> {
    let atlas = Command::new("atlas")
        .arg("--version")
        .output()
        .is_ok()
        .then(|| "atlas".to_string())
        .or_else(|| {
            Command::new("/tmp/atlas")
                .arg("--version")
                .output()
                .is_ok()
                .then(|| "/tmp/atlas".to_string())
        })
        .ok_or_else(|| "atlas not on PATH and /tmp/atlas missing".to_string())?;
    let apply = Command::new(&atlas)
        .args([
            "migrate",
            "apply",
            "--dir",
            &format!("file://{}/migrations_atlas", env!("CARGO_MANIFEST_DIR")),
            "--url",
            target_url,
        ])
        .output()
        .map_err(|e| format!("atlas migrate apply: {e}"))?;
    if !apply.status.success() {
        return Err(format!(
            "atlas migrate apply failed: {}",
            String::from_utf8_lossy(&apply.stderr)
        ));
    }

    let psk = format!("smoke-psk-{}", "x".repeat(32));
    let port = 18080 + (std::process::id() as u16) % 1000;
    let base = format!("http://127.0.0.1:{port}");
    let mut child = Command::new(warren)
        .args(["server"])
        .env("DATABASE_URL", target_url)
        .env("WARREN_ADMIN_PSK", &psk)
        .env("BIND_ADDR", format!("127.0.0.1:{port}"))
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn warren: {e}"))?;

    // Wait for /healthz. On failure, dump the child's combined log
    // so the next person debugging this test doesn't have to guess.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut healthy = false;
    while std::time::Instant::now() < deadline {
        if let Ok(out) = Command::new("curl")
            .args(["-sf", &format!("{base}/healthz")])
            .output()
        {
            if out.status.success() {
                healthy = true;
                break;
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            let mut log = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                use std::io::Read;
                let _ = stdout.read_to_string(&mut log);
            }
            if let Some(mut stderr) = child.stderr.take() {
                use std::io::Read;
                let _ = stderr.read_to_string(&mut log);
            }
            return Err(format!("warren exited early. log:\n{log}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !healthy {
        let _ = child.kill();
        return Err("warren never became healthy on /healthz within 15s".into());
    }

    // Login: POST /api/login with the PSK. Capture Set-Cookie.
    let login = Command::new("curl")
        .args(["-si", "-X", "POST", &format!("{base}/api/login")])
        .env("CURLOPT_HTTPHEADER", "ignored")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(format!(r#"{{"password":"{psk}"}}"#))
        .output()
        .map_err(|e| format!("login curl: {e}"))?;
    let login_text = String::from_utf8_lossy(&login.stdout).to_string();
    let cookie = login_text
        .lines()
        .find_map(|l| {
            l.strip_prefix("set-cookie: ")
                .or_else(|| l.strip_prefix("Set-Cookie: "))
        })
        .and_then(|l| l.split(';').next())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("no Set-Cookie on /api/login. response:\n{login_text}"))?;

    // Create an agent so the page route resolves.
    let create = Command::new("curl")
        .args([
            "-si",
            "-X",
            "POST",
            &format!("{base}/api/agents"),
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("Cookie: {cookie}"),
            "-d",
            r#"{"name":"smoke","class":"claude","model":"sonnet"}"#,
        ])
        .output()
        .map_err(|e| format!("create agent curl: {e}"))?;
    let create_text = String::from_utf8_lossy(&create.stdout).to_string();
    // HTTP response uses CRLF; split headers from body at the first
    // blank line (CRLFCRLF), with LF-only fallback for paranoid
    // robustness.
    let body = create_text
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| create_text.split("\n\n").nth(1))
        .unwrap_or("");
    let agent_id = serde_json::from_str::<serde_json::Value>(body.trim_start())
        .ok()
        .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()))
        .ok_or_else(|| format!("create agent failed: {create_text}"))?;

    // Return the child handle to the caller; they own the kill. If
    // the caller panics, the test process exiting will eventually
    // close the pipes and warren will die with them.
    Ok((base, cookie, agent_id, child))
}

fn cleanup_smoke(mut child: std::process::Child, db: &str) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = psql_exec(&admin_url(), &format!("DROP DATABASE IF EXISTS \"{db}\""));
}

/// Fetches an agent page with the admin cookie and asserts the
/// response body contains every required marker. Returns the body for
/// further assertions.
fn fetch_and_assert_page(base: &str, cookie: &str, agent_id: &str, route: &str, markers: &[&str]) {
    let url = format!("{base}/agent/{agent_id}/{route}");
    let out = Command::new("curl")
        .args(["-si", "-H", &format!("Cookie: {cookie}"), &url])
        .output()
        .expect("curl page");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let status_line = text.lines().next().unwrap_or("");
    assert!(
        status_line.contains(" 200 "),
        "{url} returned non-200: {status_line}\nfull response:\n{text}"
    );
    // Split headers from body: headers end at the first blank line.
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .or_else(|| text.split("\n\n").nth(1))
        .unwrap_or("");
    for marker in markers {
        assert!(
            body.contains(marker),
            "{url} response body is missing required marker `{marker}`. \
             This is the contract the agent pages owe the browser — if it's \
             gone, the page will render partially or as fully black."
        );
    }
}

#[test]
#[ignore]
fn http_smoke_agent_pages_render_required_assets() {
    if !has("psql") {
        eprintln!("skip: psql not on PATH");
        return;
    }
    if !has("atlas") {
        eprintln!("skip: atlas not on PATH");
        return;
    }
    if !has("curl") {
        eprintln!("skip: curl not on PATH");
        return;
    }
    let Some(warren) = warren_bin() else {
        eprintln!("skip: CARGO_BIN_EXE_warren not set (run via `cargo test`)");
        return;
    };
    if Command::new("psql")
        .args([&admin_url(), "-tAc", "SELECT 1"])
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skip: postgres unreachable at 127.0.0.1:5432");
        return;
    }

    let db = test_db_name();
    let target_url = format!("postgres://postgres@127.0.0.1:5432/{db}?sslmode=disable");
    psql_exec(&admin_url(), &format!("CREATE DATABASE \"{db}\"")).expect("create test db");

    // boot_warren_with_scratch_db leaves a warren child orphaned via
    // mem::forget so the test process can keep hitting it. We need
    // to reap it explicitly at the end; on panic we use the panic
    // unwind hook equivalent — i.e. we kill on the cleanup path.
    let boot_result = boot_warren_with_scratch_db(&warren, &target_url);
    let (base, cookie, agent_id, child) = match boot_result {
        Ok(t) => t,
        Err(e) => {
            // Drop the scratch DB; there's no warren child to kill
            // because boot_warren_with_scratch_db failed before it
            // could return one.
            let _ = psql_exec(&admin_url(), &format!("DROP DATABASE IF EXISTS \"{db}\""));
            panic!("failed to boot warren for smoke test: {e}");
        }
    };

    let markers = [
        r#"<div id="term">"#,
        r#"href="/static/vendor/xterm/xterm.css""#,
        r#"src="/static/vendor/xterm/xterm.js""#,
        r#"src="/static/vendor/xterm/xterm-addon-fit.js""#,
        "TERM_COLS",
        "new Terminal(",
        "grid-template-rows: minmax(0, 1fr)",
    ];

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fetch_and_assert_page(&base, &cookie, &agent_id, "claude", &markers);
        fetch_and_assert_page(&base, &cookie, &agent_id, "shell", &markers);
    }));

    cleanup_smoke(child, &db);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
