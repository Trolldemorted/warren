//! §A.10 `transcript_parse` — fixture-driven `TranscriptTail` tests.
//!
//! Drives the real tailer over a temp `.jsonl` file whose path is advertised
//! through `ObserverHandle` exactly as the `SessionStart` hook would, then
//! asserts the `UsageSnapshot` it emits: token counts, `context_pct_est`, and
//! the `parse_errors` counter's response to malformed lines.
//!
//! Regression guard: the tailer previously used `tx.blocking_send` inside its
//! async `run` task, which panics in an async context — so the very first
//! usage line killed the task. These tests only pass because `run` now awaits
//! `tx.send`.

use rabbit::observer::hooks::ObserverHandle;
use rabbit::observer::transcript::TranscriptTail;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// Build a tailer following `path` (as if `SessionStart` reported it), spawn
/// it, and return the receiver of usage updates.
fn start_tail(path: &std::path::Path) -> mpsc::Receiver<rabbit::observer::transcript::UsageUpdate> {
    let handle = ObserverHandle::new();
    handle.ingest(
        "SessionStart",
        &serde_json::json!({ "transcript_path": path.to_str().unwrap() }),
    );
    let fallback = PathBuf::from("/nonexistent/rabbit-test-fallback.jsonl");
    let tail = TranscriptTail::with_observer(handle, fallback);
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let _ = tail.run(tx, 20).await;
    });
    rx
}

fn write_file(path: &std::path::Path, contents: &str) {
    let mut f = std::fs::File::create(path).expect("create fixture");
    f.write_all(contents.as_bytes()).expect("write fixture");
    f.flush().expect("flush fixture");
}

#[tokio::test]
async fn emits_usage_snapshot_from_valid_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    // 60k input + 40k cache_read out of the 200k default window → 50%.
    write_file(
        &path,
        concat!(
            r#"{"message":{"role":"assistant","model":"claude-3-5-sonnet-20240620","#,
            r#""usage":{"input_tokens":60000,"output_tokens":100,"#,
            r#""cache_read_input_tokens":40000,"cache_creation_input_tokens":5}}}"#,
            "\n"
        ),
    );

    let mut rx = start_tail(&path);
    let update = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("tailer produced no update within 3s (blocking_send regression?)")
        .expect("tailer channel closed");

    let u = update.usage;
    assert_eq!(u.input_tokens, 60000);
    assert_eq!(u.output_tokens, 100);
    assert_eq!(u.cache_read, 40000);
    assert_eq!(u.cache_write, 5);
    assert_eq!(u.source, "transcript");
    assert_eq!(u.parse_errors, 0, "no malformed lines yet");
    let pct = u.context_pct_est.expect("context_pct_est present");
    assert!((pct - 50.0).abs() < 0.01, "expected ~50% context, got {pct}");
}

#[tokio::test]
async fn parse_errors_increment_on_malformed_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    // A garbage line, then a valid usage line. The snapshot from the valid
    // line must carry the accumulated parse_errors count (>= 1).
    write_file(
        &path,
        concat!(
            "this is not json at all\n",
            r#"{"message":{"role":"assistant","model":"claude-sonnet-4-20250514","#,
            r#""usage":{"input_tokens":1000,"output_tokens":10}}}"#,
            "\n"
        ),
    );

    let mut rx = start_tail(&path);
    let update = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("tailer produced no update within 3s")
        .expect("tailer channel closed");

    assert_eq!(update.usage.input_tokens, 1000);
    assert!(
        update.usage.parse_errors >= 1,
        "expected parse_errors >= 1 after a malformed line, got {}",
        update.usage.parse_errors
    );
    // Sonnet-4 uses the 1M window: 1000 / 1_000_000 → 0.1%.
    let pct = update.usage.context_pct_est.expect("context_pct_est");
    assert!((pct - 0.1).abs() < 0.01, "expected ~0.1% for sonnet-4, got {pct}");
}

#[tokio::test]
async fn lines_without_usage_produce_no_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    // A user message with no usage block, then a valid assistant usage line.
    // The first line must be silently skipped; the first (and only) update we
    // receive is the assistant one.
    write_file(
        &path,
        concat!(
            r#"{"message":{"role":"user"}}"#,
            "\n",
            r#"{"message":{"role":"assistant","model":"m","usage":{"input_tokens":7}}}"#,
            "\n"
        ),
    );

    let mut rx = start_tail(&path);
    let update = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("no update within 3s")
        .expect("channel closed");
    assert_eq!(
        update.usage.input_tokens, 7,
        "the first emitted update should be the assistant line, not the usage-less user line"
    );
}
