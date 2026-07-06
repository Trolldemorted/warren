//! Shared helpers for the rabbit integration tests.
//!
//! These drive a real PTY (via `rabbit::pty::Pty`) with a fake TUI child
//! (`/bin/cat`, which echoes stdin to stdout) so the byte-pump and input
//! discipline can be observed the way a real terminal consumer would see
//! them — without spawning the actual `claude` binary or hitting the API.

#![allow(dead_code)]

use rabbit::pty::Pty;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// A fake TUI that echoes everything written to it, standing in for `claude`.
pub const FAKE_TUI: &str = "/bin/cat";

/// Spawn a `Pty` running `/bin/cat` at the given size. Panics on failure —
/// these are tests and a spawn failure means the environment is unusable.
pub fn spawn_fake_tui(cols: u16, rows: u16) -> Pty {
    Pty::spawn(FAKE_TUI, &[], ".", cols, rows, 4096).expect("spawn fake TUI (/bin/cat)")
}

/// Start a background thread draining the PTY master reader into a channel of
/// byte chunks. Returns the receiver; the thread exits on EOF or channel drop.
pub fn spawn_reader(pty: &Pty) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut reader = pty.reader();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Accumulate bytes from `rx` until `needle` appears in the running buffer or
/// `timeout` elapses. Returns `(found, accumulated_bytes)`.
pub fn read_until(rx: &Receiver<Vec<u8>>, needle: &[u8], timeout: Duration) -> (bool, Vec<u8>) {
    let deadline = Instant::now() + timeout;
    let mut acc: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                if contains(&acc, needle) {
                    return (true, acc);
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    (contains(&acc, needle), acc)
}

/// True if `haystack` contains `needle` as a contiguous byte subsequence.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
