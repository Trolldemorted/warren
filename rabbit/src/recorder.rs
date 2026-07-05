//! §D Milestone 5 — asciicast v2 recording sidecar.
//!
//! [`AsciicastRecorder`] is a passive observer on the claude PTY byte stream,
//! mirroring the same job [`crate::vt::TermTracker`] does for the live VT
//! state, but persisting to disk instead. Every read chunk becomes a
//! `[<time_offset>, "o", "<utf8-lossy bytes>"]` line in asciicast v2 JSONL,
//! plus a single header line at session start with `version`, `width`,
//! `height`, `timestamp`, and a small `env` map.
//!
//! ## Why this lives in the driver task, not the blocking PTY thread
//!
//! The recorder holds a `tokio::fs::File` and only sees the byte stream
//! once it has crossed the `PtyEvt::Read` channel — that's the same stream
//! the replay buffer + live WS subscribers see, so there's exactly one
//! source of truth and no double recording. The blocking thread stays
//! focused on PTY I/O and never blocks on file writes.
//!
//! ## Lifetime
//!
//! One recorder per claude generation. `start_session` opens a fresh
//! `.cast` file for each `session_id` (so a `--continue` rotation produces
//! `<id>.cast`, `<id>.cast.0`, …). `close` flushes and drops the handle.
//! After the first I/O error the recorder enters a `broken` state and
//! suppresses further writes — never propagates the error to the caller.
//!
//! ## Rotation
//!
//! When `current_size + incoming_chunk > cap`, the recorder rotates:
//! `<id>.cast` → `<id>.cast.0`, existing `.cast.0` → `.cast.1`, …, with
//! the oldest segment evicted past `MAX_ROTATION_DEPTH` (64). The cap is
//! applied to the current segment only, not the cumulative session size.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

/// Maximum rotation depth per session. Bounds the rename storm in
/// pathological cases (cap very small, output very large).
const MAX_ROTATION_DEPTH: u32 = 64;

struct CurrentFile {
    session_id: String,
    file: File,
    bytes: u64,
    start: Instant,
}

/// Asciicast v2 sidecar recorder. Holds at most one open file at a time.
pub struct AsciicastRecorder {
    dir: PathBuf,
    cap: u64,
    cols: u16,
    rows: u16,
    current: Option<CurrentFile>,
    /// Set after the first I/O error. Subsequent feeds become no-ops so
    /// a broken recorder can never propagate errors into the driver loop.
    broken: bool,
    /// Emitted once per recorder lifetime, the first time a feed happens
    /// before any session_start event has been seen. Avoids log spam for
    /// every pre-session chunk (rabbit starts feeding bytes before the
    /// observer fires the SessionStart hook).
    warned_no_session: bool,
}

impl AsciicastRecorder {
    pub fn new(dir: PathBuf, cap: u64, cols: u16, rows: u16) -> Self {
        Self {
            dir,
            cap,
            cols,
            rows,
            current: None,
            broken: false,
            warned_no_session: false,
        }
    }

    /// Open a new `.cast` file for `session_id`, write the v2 header, and
    /// drop any prior current file (flushes it). No-op if the recorder is
    /// already broken.
    pub async fn start_session(&mut self, session_id: &str) -> Result<()> {
        if self.broken {
            return Ok(());
        }
        // Flush + drop any prior session. Doing it explicitly (rather than
        // relying on Drop) lets us catch the I/O error and mark broken.
        if let Some(mut cf) = self.current.take() {
            let sid = cf.session_id.clone();
            if let Err(e) = cf.file.flush().await {
                log::warn!("asciicast: closing prior session {sid} failed: {e:?}");
                self.broken = true;
                return Ok(());
            }
        }

        if let Err(e) = tokio::fs::create_dir_all(&self.dir).await {
            log::warn!("asciicast: create_dir_all({}) failed: {e:?}", self.dir.display());
            self.broken = true;
            return Ok(());
        }

        let path = self.path_for(session_id);
        let file = match OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                log::warn!("asciicast: open({}) failed: {e:?}", path.display());
                self.broken = true;
                return Ok(());
            }
        };

        let started_unix = chrono::Utc::now().timestamp();
        let header = build_header(self.cols, self.rows, started_unix);
        let mut file = file;
        if let Err(e) = file.write_all(header.as_bytes()).await {
            log::warn!("asciicast: header write failed: {e:?}");
            self.broken = true;
            return Ok(());
        }
        if let Err(e) = file.flush().await {
            log::warn!("asciicast: header flush failed: {e:?}");
            self.broken = true;
            return Ok(());
        }

        self.current = Some(CurrentFile {
            session_id: session_id.to_string(),
            file,
            bytes: header.len() as u64,
            start: Instant::now(),
        });
        self.warned_no_session = false;
        Ok(())
    }

    /// Record one chunk of PTY output. Drops the chunk (with a single
    /// warn) if no session has started yet. After a rotation-triggering
    /// write, the prior `.cast` is renamed to `.cast.0` and a new one
    /// opens. After any I/O error, the recorder marks itself broken and
    /// becomes a no-op for the rest of its lifetime.
    pub async fn feed(&mut self, chunk: &[u8]) {
        if self.broken || chunk.is_empty() {
            return;
        }
        // Snapshot the projected frame length under no self-borrow, so the
        // subsequent `rotate()` call (which needs `&mut self`) doesn't
        // fight with the cf borrow.
        let needs_rotate = match self.current.as_ref() {
            Some(cf) => {
                let projected_bytes = cf.bytes.saturating_add(line_size_estimate(chunk));
                projected_bytes > self.cap
            }
            None => false,
        };
        if self.current.is_none() {
            if !self.warned_no_session {
                log::debug!(
                    "asciicast: dropping pre-session chunk ({} bytes)",
                    chunk.len()
                );
                self.warned_no_session = true;
            }
            return;
        }

        if needs_rotate {
            // Take the current out so we can hand the file to rotate_inner
            // (which returns a fresh handle), then re-install with reset
            // bookkeeping.
            let cf = self
                .current
                .take()
                .expect("needs_rotate implies current is Some");
            let sid = cf.session_id.clone();
            match rotate_inner(&self.dir, &sid, cf.file).await {
                Ok(new_file) => {
                    self.current = Some(CurrentFile {
                        session_id: sid,
                        file: new_file,
                        bytes: 0,
                        start: Instant::now(),
                    });
                }
                Err(e) => {
                    log::warn!("asciicast: rotate failed: {e:?}");
                    self.broken = true;
                    return;
                }
            }
            if let Err(e) = rewrite_header_after_rotation(
                self.cols,
                self.rows,
                self.current.as_mut().expect("current set by rotate"),
            )
            .await
            {
                log::warn!("asciicast: header rewrite failed: {e:?}");
                self.broken = true;
                return;
            }
        }

        // Compute the final line under no self-borrow, then re-borrow to write.
        let line = {
            let cf = self.current.as_ref().expect("current set above");
            format_output_line(cf.start.elapsed().as_secs_f64(), chunk)
        };
        let cf = self
            .current
            .as_mut()
            .expect("current set above; rotation preserves it");
        if let Err(e) = cf.file.write_all(line.as_bytes()).await {
            log::warn!("asciicast: write failed: {e:?}");
            self.broken = true;
            return;
        }
        cf.bytes = cf.bytes.saturating_add(line.len() as u64);
    }

    /// Flush + drop the current file (if any). Idempotent.
    pub async fn close(&mut self) {
        if let Some(cf) = self.current.take() {
            if let Err(e) = flush_close(cf).await {
                log::warn!("asciicast: close failed: {e:?}");
                self.broken = true;
            }
        }
    }

    /// True iff an earlier I/O error marked this recorder unsalvageable.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Path of the live file for the current session, or `None` if no
    /// session is open. Useful for tests / diagnostics.
    pub fn current_path(&self) -> Option<PathBuf> {
        let cf = self.current.as_ref()?;
        Some(self.dir.join(format!("{}.cast", cf.session_id)))
    }

    fn path_for(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.cast"))
    }
}

/// Estimate the on-disk size of a frame line without allocating the full
/// string. Cheap upper bound — actual size is within a few bytes due to
/// JSON escaping and UTF-8 replacement chars.
fn line_size_estimate(chunk: &[u8]) -> u64 {
    // Format: `[<float>,"o","<data>"]\n`. The float is at most ~24 chars
    // (Rust default Debug for f64), plus the literal 10 chars for the
    // framing and the newline.
    let base: u64 = 10 + 24 + 1;
    // Worst case: every byte becomes a 6-char `\u00XX` escape. Lossy
    // UTF-8 conversion can produce 3-byte chars but never grows more than
    // 6x under our escape rule.
    base + (chunk.len() as u64) * 6
}

/// Rotate the current file: shift `.cast.N-1` → `.cast.N` (evicting
/// past `MAX_ROTATION_DEPTH`), rename current `.cast` → `.cast.0`,
/// then reopen `.cast` (truncated). Caller is responsible for
/// rewriting the header into the new file.
async fn rotate_inner(dir: &Path, session_id: &str, mut file: File) -> Result<File> {
    // Flush + drop the open file handle so the rename is unproblematic
    // on platforms that don't allow renaming an open file.
    if let Err(e) = file.flush().await {
        log::warn!("asciicast: pre-rotate flush failed: {e:?}");
    }
    drop(file);

    let live = dir.join(format!("{session_id}.cast"));
    // Shift older segments up: .cast.N-1 → .cast.N. Evict the oldest
    // past MAX_ROTATION_DEPTH.
    for n in (1..MAX_ROTATION_DEPTH).rev() {
        let from = dir.join(format!("{session_id}.cast.{n}"));
        if from.exists() {
            let to = dir.join(format!("{session_id}.cast.{}", n + 1));
            if to.exists() {
                let _ = tokio::fs::remove_file(&to).await;
            }
            let _ = tokio::fs::rename(&from, &to).await;
        }
    }
    // Move live → .cast.0, evicting .cast.MAX_ROTATION_DEPTH if present.
    let oldest = dir.join(format!("{session_id}.cast.{MAX_ROTATION_DEPTH}"));
    if oldest.exists() {
        let _ = tokio::fs::remove_file(&oldest).await;
    }
    let target = dir.join(format!("{session_id}.cast.0"));
    if target.exists() {
        let _ = tokio::fs::remove_file(&target).await;
    }
    tokio::fs::rename(&live, &target).await?;

    // Reopen the live file (truncated).
    let new_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&live)
        .await?;
    Ok(new_file)
}

async fn rewrite_header_after_rotation(
    cols: u16,
    rows: u16,
    cf: &mut CurrentFile,
) -> Result<()> {
    let started_unix = chrono::Utc::now().timestamp();
    let header = build_header(cols, rows, started_unix);
    cf.file.write_all(header.as_bytes()).await?;
    cf.file.flush().await?;
    cf.bytes = header.len() as u64;
    Ok(())
}

async fn flush_close(mut cf: CurrentFile) -> Result<()> {
    cf.file.flush().await?;
    // Dropping closes; explicit for clarity.
    drop(cf.file);
    Ok(())
}

fn build_header(cols: u16, rows: u16, timestamp: i64) -> String {
    let env_json = r#"{"SHELL":"/bin/bash","TERM":"xterm-256color"}"#;
    format!(
        "{{\"version\":2,\"width\":{cols},\"height\":{rows},\"timestamp\":{timestamp},\"env\":{env}}}\n",
        cols = cols,
        rows = rows,
        timestamp = timestamp,
        env = env_json,
    )
}

fn format_output_line(offset_secs: f64, chunk: &[u8]) -> String {
    let data = String::from_utf8_lossy(chunk);
    let escaped = json_escape_string(&data);
    format!("[{:.6},\"o\",\"{}\"]\n", offset_secs, escaped)
}

/// JSON-escape a string for embedding inside a `"…"` JSON literal.
/// Mirrors RFC 8259 §7 except we emit `\u00XX` for control characters
/// rather than the optional shorter escapes; asciinema-player accepts
/// either.
fn json_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// (Removed dead `_RESERVED` / `_path_exists` stubs.)

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rabbit-asciicast-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[tokio::test]
    async fn writes_v2_header_and_output_frames() {
        let dir = tmpdir("basic");
        let mut rec = AsciicastRecorder::new(dir.clone(), 1024 * 1024, 80, 24);
        rec.start_session("sess1").await.unwrap();
        rec.feed(b"hello\n").await;
        // Explicit close so a sync read_to_string from another task can't
        // race the async write. Same dance as the other tests.
        rec.close().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let content = std::fs::read_to_string(dir.join("sess1.cast")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected header + 1 frame, got {lines:?}");
        let header = lines[0];
        assert!(header.starts_with("{\"version\":2,\"width\":80,\"height\":24,"));
        assert!(header.contains(r#""timestamp":"#));
        assert!(header.ends_with('}'));
        // Output line: [<float>,"o","<escaped data>"]
        let frame = lines[1];
        assert!(frame.starts_with('['));
        assert!(frame.contains(r#","o","hello\n""#));
        assert!(frame.ends_with(']'));
    }

    #[tokio::test]
    async fn rotates_on_size_cap() {
        let dir = tmpdir("rotate");
        // Tiny cap so each ~50-byte frame triggers a rotation.
        let mut rec = AsciicastRecorder::new(dir.clone(), 90, 80, 24);
        rec.start_session("s1").await.unwrap();
        rec.feed(b"frame1-padding-padding-padding-padding").await;
        rec.feed(b"frame2-padding-padding-padding-padding").await;
        rec.feed(b"frame3-padding-padding-padding-padding").await;
        rec.close().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Live file + at least one rotated segment.
        assert!(dir.join("s1.cast").exists(), "live file missing");
        assert!(
            dir.join("s1.cast.0").exists(),
            "expected first rotation segment, dir={:?}",
            std::fs::read_dir(&dir).unwrap().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn replaces_invalid_utf8_with_replacement_char() {
        let dir = tmpdir("utf8");
        let mut rec = AsciicastRecorder::new(dir.clone(), 1024, 80, 24);
        rec.start_session("u").await.unwrap();
        rec.feed(&[b'a', 0xFF, b'b']).await;
        rec.close().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let content = std::fs::read_to_string(dir.join("u.cast")).unwrap();
        // Should contain U+FFFD inside the JSON string.
        assert!(content.contains('\u{FFFD}'), "no replacement char in: {content:?}");
    }

    #[tokio::test]
    async fn start_session_closes_prior_and_opens_new() {
        let dir = tmpdir("multi");
        let mut rec = AsciicastRecorder::new(dir.clone(), 1024 * 1024, 80, 24);
        rec.start_session("a").await.unwrap();
        rec.feed(b"first").await;
        rec.start_session("b").await.unwrap();
        rec.feed(b"second").await;
        rec.close().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let a = std::fs::read_to_string(dir.join("a.cast")).unwrap();
        let b = std::fs::read_to_string(dir.join("b.cast")).unwrap();
        assert!(a.contains("\"o\",\"first\""), "first session lost: {a}");
        assert!(b.contains("\"o\",\"second\""), "second session lost: {b}");
    }

    #[tokio::test]
    async fn broken_after_io_error_does_not_panic() {
        // Point at a path under /proc (not writable).
        let dir = PathBuf::from("/proc/1/cannot-write-here");
        let mut rec = AsciicastRecorder::new(dir, 1024, 80, 24);
        rec.start_session("x").await.unwrap();
        // start_session should have marked us broken silently.
        assert!(rec.is_broken(), "expected broken after unwritable dir");
        rec.feed(b"data").await;
        // Subsequent feed must be a no-op, no panic.
        assert!(rec.is_broken());
    }

    #[tokio::test]
    async fn empty_chunk_is_noop() {
        let dir = tmpdir("empty");
        let mut rec = AsciicastRecorder::new(dir.clone(), 1024, 80, 24);
        rec.start_session("e").await.unwrap();
        rec.feed(b"").await;
        rec.close().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let content = std::fs::read_to_string(dir.join("e.cast")).unwrap();
        // Just the header, no extra frame.
        assert_eq!(content.lines().count(), 1, "got: {content:?}");
    }

    #[test]
    fn json_escape_handles_control_chars_and_quotes() {
        assert_eq!(json_escape_string("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(json_escape_string("\x01"), "\\u0001");
        assert_eq!(json_escape_string("\t"), "\\t");
    }
}