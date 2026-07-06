//! Smoke test: spawn the actual `claude` binary in a PTY, strip the
//! nested-Code env markers, and push a small prompt via the §A.2
//! `input::paste` byte sequence.
//!
//! Marked `#[ignore]` so `cargo test -p rabbit` doesn't run it (it hits the
//! Anthropic API and takes ~20–60s wall-clock per run).
//!
//! Run explicitly:
//!   cargo test -p rabbit --test claude_smoke -- --ignored --nocapture claude_pt_roundtrip

use rabbit_lib::input::paste;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::Read;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const PROMPT: &str = "say just the word 'pong' and nothing else";
const TIMEOUT: Duration = Duration::from_secs(60);
const BOOT_WAIT: Duration = Duration::from_secs(4);
const QUIET: Duration = Duration::from_secs(8);
const FIRST_BYTE: Duration = Duration::from_secs(45);

struct ChildPty {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl ChildPty {
    fn reader(&self) -> Box<dyn Read + Send> {
        self.master.try_clone_reader().expect("clone reader")
    }

    fn writer(&self) -> Box<dyn std::io::Write + Send> {
        self.master.take_writer().expect("take writer")
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
    }

    fn wait(&mut self) {
        let _ = self.child.wait();
    }
}

/// Trust-dialog detection uses the production markers in `rabbit_lib::trust` so
/// the smoke test and the supervisor's auto-accept path can never drift apart.
/// The smoke test detects any marker in the PTY output and accepts the dialog
/// by sending Enter, then re-sends the prompt.
use rabbit_lib::trust::has_trust_marker;

/// Spawn `claude` with the nested-Code env markers stripped. claude reads
/// `CLAUDECODE` and `CLAUDE_CODE_*` to detect "I'm being driven by another
/// Claude Code instance" and refuses to run nested prompts; without
/// stripping, a smoke test inside a Claude Code shell would only observe
/// the child no-op'ing.
fn spawn_claude_stripped(bin: &str, args: &[String], workdir: &str) -> std::io::Result<ChildPty> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            cols: 120,
            rows: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::other(format!("openpty: {e}")))?;
    let mut cmd = CommandBuilder::new(bin);
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(workdir);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Strip nested-Code markers so the child claude runs as if invoked from
    // a fresh shell. Without this, the child silently refuses to handle
    // programmatic prompts.
    for k in [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "CLAUDE_EFFORT",
    ] {
        cmd.env_remove(k);
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| std::io::Error::other(format!("spawn: {e}")))?;
    drop(pair.slave);
    Ok(ChildPty {
        master: pair.master,
        child,
    })
}

#[test]
#[ignore]
fn claude_pt_roundtrip() {
    let workdir = format!("/tmp/rabbit-smoke-{}", std::process::id());
    std::fs::create_dir_all(&workdir).expect("mkdir workdir");

    let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let args = vec!["--dangerously-skip-permissions".to_string()];

    eprintln!("[smoke] spawning {bin} {args:?} in {workdir}");
    let mut pty = spawn_claude_stripped(&bin, &args, &workdir).expect("spawn claude");

    // Dedicated reader thread — blocking PTY reads can't be timed out in-line.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader = pty.reader();
    let tx_worker = tx.clone();
    let worker = thread::spawn(move || {
        let mut r = reader;
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx_worker.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let writer = pty.writer();

    // Drain the pre-prompt output (TUI banner, possibly a trust dialog).
    // If a trust dialog appears, accept it with Enter before sending the
    // actual prompt — otherwise the dialog swallows our paste as its
    // "yes" answer and the real prompt never reaches the model.
    std::thread::sleep(BOOT_WAIT);
    let mut pre = Vec::new();
    let pre_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < pre_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => pre.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    eprintln!("[smoke] pre-prompt bytes: {}", pre.len());
    if !pty.alive() {
        drop(writer);
        pty.terminate();
        pty.wait();
        let dump = strip_ansi(&pre);
        panic!("claude exited during boot; pre-prompt:\n{dump}");
    }

    let pre_text = strip_ansi(&pre);
    let mut w = writer;
    if has_trust_marker(&pre_text) {
        eprintln!("[smoke] trust dialog detected in pre-prompt; accepting with Enter");
        let _ = w.write_all(b"\r");
        let _ = w.flush();
        std::thread::sleep(Duration::from_millis(800));
    }

    // Push the prompt through §A.2's bracketed-paste sequence.
    eprintln!("[smoke] sending prompt via input::paste: {PROMPT:?}");
    paste(&mut w, PROMPT).expect("paste");

    // Read response, exit on quiet OR first-byte timeout. If the response
    // contains a trust marker, our paste got consumed by the dialog —
    // accept it and re-send the prompt.
    let mut response = Vec::new();
    let start = Instant::now();
    let first_byte_deadline = start + FIRST_BYTE;
    let mut last_new: Option<Instant> = None;
    let mut prompt_retries: u8 = 0;
    while start.elapsed() < TIMEOUT {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                response.extend_from_slice(&chunk);
                last_new = Some(Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(t) = last_new {
                    if t.elapsed() >= QUIET && !response.is_empty() {
                        break;
                    }
                }
                if response.is_empty() && Instant::now() >= first_byte_deadline {
                    eprintln!("[smoke] first-byte timeout at t={:?}", start.elapsed());
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if !pty.alive() {
            eprintln!("[smoke] claude exited at t={:?}", start.elapsed());
            break;
        }
        // If a trust dialog appeared in the post-prompt output, accept it
        // and re-send the prompt. Bounded retries to avoid an infinite
        // loop if claude keeps prompting for trust.
        let combined = strip_ansi(&response);
        if has_trust_marker(&combined) && prompt_retries < 2 {
            eprintln!(
                "[smoke] trust marker in response (retry {}); accepting + resending",
                prompt_retries
            );
            prompt_retries += 1;
            let _ = w.write_all(b"\r");
            let _ = w.flush();
            std::thread::sleep(Duration::from_secs(1));
            response.clear();
            last_new = None;
            paste(&mut w, PROMPT).expect("re-paste");
        }
    }

    // Tear down.
    drop(w);
    pty.terminate();
    pty.wait();
    drop(tx);
    let _ = worker.join();

    eprintln!(
        "[smoke] response bytes: {}; elapsed {:?}",
        response.len(),
        start.elapsed()
    );
    let resp_text = strip_ansi(&response);
    let tail: String = resp_text
        .chars()
        .rev()
        .take(600)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    eprintln!("[smoke] response tail (ansi-stripped, last 600 chars):");
    eprintln!("---8<---\n{tail}\n---8<---");

    let pong = resp_text.to_ascii_lowercase().contains("pong");
    eprintln!("[smoke] contains 'pong'? {pong}");
    eprintln!(
        "[smoke] response contains 'respond'? {}",
        resp_text.to_ascii_lowercase().contains("respond")
    );
    // Dump the full response for debugging when the prompt doesn't land.
    if !pong {
        eprintln!("[smoke] FULL response text (ansi-stripped):");
        eprintln!("===8<===");
        for line in resp_text.lines() {
            if !line.trim().is_empty() {
                eprintln!("{line}");
            }
        }
        eprintln!("===8<===");
    }

    assert!(
        !response.is_empty(),
        "no response bytes captured within timeout"
    );
    assert!(
        pong,
        "claude did not include 'pong' in its response — bracketed paste may not have landed"
    );
}

fn strip_ansi(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            if chars.peek() == Some(&']') {
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '\x07' {
                        break;
                    }
                    if nc == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
                continue;
            }
            continue;
        }
        if c == '\r' {
            out.push('\n');
            continue;
        }
        out.push(c);
    }
    out
}

// ─── §A.10 milestone-1 checklist ──────────────────────────────────────────────
//
// The `claude_pt_roundtrip` test above exercises the bracketed-paste path
// against the trust dialog. The three tests below cover the rest of the
// milestone-1 acceptance checklist called out in `input.rs`:
//   * `/clear` resets the transcript and re-arms the input prompt
//   * `ESC` interrupts a mid-response turn without crashing claude
//   * `/usage` slash command yields a context/usage readout
//
// Each test is self-contained: spawn a fresh `claude` PTY, exercise the
// scenario, tear down. All `#[ignore]`d so `cargo test -p rabbit` doesn't
// hit the Anthropic API in CI.

// Shared smoke harness: spawns claude, drains the boot banner (and accepts
// any trust dialog via `rabbit_lib::trust::has_trust_marker`), then exposes a
// writer + receiver pair for the test to drive.
struct SmokeHarness {
    pty: ChildPty,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn std::io::Write + Send>,
}

fn spawn_smoke_harness(workdir: &str) -> SmokeHarness {
    let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let args = vec!["--dangerously-skip-permissions".to_string()];
    eprintln!("[smoke-harness] spawning {bin} {args:?} in {workdir}");
    let pty = spawn_claude_stripped(&bin, &args, workdir).expect("spawn claude");

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader = pty.reader();
    let tx_worker = tx.clone();
    // Worker runs detached: dropping the JoinHandle is fine because the test
    // relies on the channel staying open via the worker's owned clone of
    // `tx_worker`. The thread exits naturally when the PTY EOFs (after
    // `shutdown`) or when the receiver is dropped at test end.
    let _worker = thread::spawn(move || {
        let mut r = reader;
        let mut buf = [0u8; 4096];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx_worker.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut writer = pty.writer();
    // Drain boot + accept any trust dialog the same way `claude_pt_roundtrip`
    // does, so the trust marker never makes these tests flake.
    std::thread::sleep(BOOT_WAIT);
    let mut pre = Vec::new();
    let pre_deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < pre_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => pre.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if has_trust_marker(&strip_ansi(&pre)) {
        eprintln!("[smoke-harness] trust dialog detected; accepting with Enter");
        let _ = writer.write_all(b"\r");
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(800));
    }

    SmokeHarness {
        pty,
        rx,
        writer,
    }
}

impl SmokeHarness {
    fn paste(&mut self, text: &str) {
        paste(&mut self.writer, text).expect("paste");
    }

    fn slash(&mut self, cmd: &str) {
        rabbit_lib::input::slash(&mut self.writer, cmd).expect("slash");
    }

    fn interrupt(&mut self) {
        rabbit_lib::input::interrupt(&mut self.writer).expect("interrupt");
    }

    /// Drain the receiver for `dur`. Returns the ANSI-stripped text so
    /// callers can assert on it.
    fn read_for(&mut self, dur: Duration) -> String {
        let mut buf = Vec::new();
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(c) => buf.extend_from_slice(&c),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        strip_ansi(&buf)
    }

    fn alive(&mut self) -> bool {
        self.pty.alive()
    }

    fn shutdown(mut self) {
        drop(self.writer);
        self.pty.terminate();
        self.pty.wait();
    }
}

/// `/clear` should reset the TUI to a fresh input prompt: after sending
/// `/clear`, a follow-up prompt must reach claude (i.e. we get a fresh
/// response). We don't observe the transcript file directly from the smoke
/// test (that's covered by `transcript_parse.rs`); we observe the *wire*
/// behavior — claude accepts and answers a second prompt.
#[test]
#[ignore]
fn clear_resets_terminal_and_rearms_prompt() {
    let workdir = format!("/tmp/rabbit-smoke-clear-{}", std::process::id());
    std::fs::create_dir_all(&workdir).expect("mkdir workdir");
    let mut h = spawn_smoke_harness(&workdir);

    h.paste("say the word 'before-clear' and nothing else");
    let resp1 = h.read_for(Duration::from_secs(45));
    assert!(
        resp1.to_ascii_lowercase().contains("before-clear"),
        "first prompt response missing 'before-clear'; got tail: {}",
        tail(&resp1, 400)
    );

    h.slash("clear");
    // Give the TUI a moment to redraw, then verify a *new* prompt still
    // reaches the model. claude's banner has a `>` input row after `/clear`.
    std::thread::sleep(Duration::from_secs(2));
    h.paste("say the word 'after-clear' and nothing else");
    let resp2 = h.read_for(Duration::from_secs(45));
    assert!(
        resp2.to_ascii_lowercase().contains("after-clear"),
        "/clear broke the input path; got tail: {}",
        tail(&resp2, 400)
    );

    h.shutdown();
}

/// ESC mid-response: send a prompt engineered to take a few seconds, give
/// the model a head-start, then send ESC. The agent should *not* crash and
/// should accept a follow-up prompt (i.e. ESC cleanly interrupts rather than
/// tearing down the session).
#[test]
#[ignore]
fn esc_interrupt_does_not_crash_session() {
    let workdir = format!("/tmp/rabbit-smoke-esc-{}", std::process::id());
    std::fs::create_dir_all(&workdir).expect("mkdir workdir");
    let mut h = spawn_smoke_harness(&workdir);

    h.paste("write a five-line story about a rabbit, then stop");
    // Wait long enough for some output to start streaming.
    let _ = h.read_for(Duration::from_secs(3));
    assert!(h.alive(), "claude died before we could send ESC");

    h.interrupt();
    // After ESC, claude should redraw the input prompt within a few seconds.
    let post = h.read_for(Duration::from_secs(3));
    assert!(
        h.alive(),
        "claude crashed after ESC — got tail: {}",
        tail(&post, 400)
    );

    // Sanity: a follow-up prompt still reaches the model.
    h.paste("say the word 'survived' and nothing else");
    let resp = h.read_for(Duration::from_secs(30));
    assert!(
        resp.to_ascii_lowercase().contains("survived"),
        "follow-up prompt after ESC didn't land; got tail: {}",
        tail(&resp, 400)
    );

    h.shutdown();
}

/// `/usage` slash command should produce some kind of usage/context readout.
/// We don't pin the exact format (claude renders this as a styled TUI panel
/// that varies by version), only that *something* related to context/usage
/// shows up after the slash is sent.
#[test]
#[ignore]
fn slash_usage_returns_context_readout() {
    let workdir = format!("/tmp/rabbit-smoke-usage-{}", std::process::id());
    std::fs::create_dir_all(&workdir).expect("mkdir workdir");
    let mut h = spawn_smoke_harness(&workdir);

    h.slash("usage");
    let out = h.read_for(Duration::from_secs(15));
    let lower = out.to_ascii_lowercase();
    // claude's /usage panel mentions at least one of: usage, context,
    // tokens. We allow any of them so the test doesn't lock to a specific
    // label that future versions might rename.
    let has_signal = lower.contains("usage")
        || lower.contains("context")
        || lower.contains("token");
    assert!(
        has_signal,
        "/usage produced no usage/context/token signal; got tail: {}",
        tail(&out, 400)
    );

    h.shutdown();
}

fn tail(s: &str, n: usize) -> String {
    s.chars().rev().take(n).collect::<String>().chars().rev().collect()
}
