//! §A.10 `input_discipline` — bracketed-paste / slash / ESC against a real PTY.
//!
//! The byte-exact sequences are pinned by `input::tests` (unit level). This
//! file is the black-box complement: it drives the `input::*` writers into a
//! real PTY master and asserts a real terminal consumer (`/bin/cat`, in the
//! kernel line discipline) actually receives the payload — i.e. the sequences
//! survive a real tty, not just a `Vec<u8>`.

mod common;

use common::{contains, read_until, spawn_fake_tui, spawn_reader};
use rabbit_lib::input;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Drain everything available from `rx` for `dur`, returning the bytes seen.
fn drain_for(rx: &Receiver<Vec<u8>>, dur: Duration) -> Vec<u8> {
    let deadline = Instant::now() + dur;
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => acc.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    acc
}

#[test]
fn paste_payload_reaches_the_pty_consumer() {
    let mut pty = spawn_fake_tui(120, 40);
    let rx = spawn_reader(&pty);
    let mut w = pty.writer();

    // paste() ends with ENTER (\r), which flushes the canonical line buffer.
    input::paste(&mut w, "DISCIPLINE-TOKEN-7").expect("paste");

    let (found, acc) = read_until(&rx, b"DISCIPLINE-TOKEN-7", TIMEOUT);
    let _ = pty.terminate();
    let _ = pty.wait();

    assert!(
        found,
        "paste payload never reached the PTY consumer; saw: {:?}",
        String::from_utf8_lossy(&acc)
    );
}

#[test]
fn slash_command_reaches_the_pty_consumer() {
    let mut pty = spawn_fake_tui(120, 40);
    let rx = spawn_reader(&pty);
    let mut w = pty.writer();

    input::slash(&mut w, "usage").expect("slash");

    let (found, acc) = read_until(&rx, b"/usage", TIMEOUT);
    let _ = pty.terminate();
    let _ = pty.wait();

    assert!(
        found,
        "slash command '/usage' never reached the PTY consumer; saw: {:?}",
        String::from_utf8_lossy(&acc)
    );
}

#[test]
fn interrupt_sends_ctrl_c_to_the_pty() {
    let mut pty = spawn_fake_tui(120, 40);
    let rx = spawn_reader(&pty);
    let mut w = pty.writer();

    input::interrupt(&mut w).expect("interrupt");

    // The tty echoes the Ctrl-C byte immediately. Depending on ECHOCTL
    // it appears as a raw 0x03 or the caret form "^C" — accept either.
    let acc = drain_for(&rx, Duration::from_millis(800));
    let _ = pty.terminate();
    let _ = pty.wait();

    assert!(
        contains(&acc, b"\x03") || contains(&acc, b"^C"),
        "interrupt (Ctrl-C byte) did not reach the PTY; saw {} bytes: {:?}",
        acc.len(),
        String::from_utf8_lossy(&acc)
    );
}
