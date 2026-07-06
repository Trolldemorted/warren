//! §A.10 `pty_echo` — PTY byte-pump + replay buffer against a fake TUI.
//!
//! Complements the in-crate `pty::tests` (which cover the jiggle math) with a
//! black-box round-trip: bytes written to the master must reach a real child
//! and come back out the reader. The fake TUI is `/bin/cat` (echoes stdin).

mod common;

use common::{read_until, spawn_fake_tui, spawn_reader};
use rabbit::pty::Pty;
use std::io::Write;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn byte_pump_round_trips_through_real_pty() {
    let mut pty = spawn_fake_tui(120, 40);
    let rx = spawn_reader(&pty);
    let mut w = pty.writer();

    // A newline flushes the tty's canonical line buffer so `cat` echoes it.
    w.write_all(b"PONGTOKEN-42\n").expect("write to pty");
    w.flush().expect("flush");

    let (found, acc) = read_until(&rx, b"PONGTOKEN-42", TIMEOUT);
    let _ = pty.terminate();
    let _ = pty.wait();

    assert!(
        found,
        "wrote PONGTOKEN-42 to the PTY but never observed it echoed back; got {} bytes: {:?}",
        acc.len(),
        String::from_utf8_lossy(&acc)
    );
}

#[test]
fn multiple_writes_all_arrive_in_order() {
    let mut pty = spawn_fake_tui(120, 40);
    let rx = spawn_reader(&pty);
    let mut w = pty.writer();

    w.write_all(b"alpha\n").unwrap();
    w.flush().unwrap();
    // Second marker after the first is observed, to prove the pump keeps going.
    let (found_a, _) = read_until(&rx, b"alpha", TIMEOUT);
    w.write_all(b"omega\n").unwrap();
    w.flush().unwrap();
    let (found_o, acc) = read_until(&rx, b"omega", TIMEOUT);

    let _ = pty.terminate();
    let _ = pty.wait();

    assert!(found_a, "first marker 'alpha' never echoed");
    assert!(
        found_o,
        "second marker 'omega' never echoed; got: {:?}",
        String::from_utf8_lossy(&acc)
    );
}

#[test]
fn replay_buffer_snapshots_pushed_bytes_under_cap() {
    // No child needed for the replay buffer contract; spawn a short-lived one
    // just to obtain a Pty. The replay VecDeque is independent of the child.
    let pty = Pty::spawn(
        "/bin/sh",
        &["-c".into(), "sleep 0.2".into()],
        ".",
        80,
        24,
        4096,
    )
    .expect("spawn sh");
    pty.push_replay(b"hello ");
    pty.push_replay(b"world");
    let snap = pty.snapshot_replay();
    assert_eq!(
        &snap[..],
        b"hello world",
        "snapshot must return pushed bytes"
    );
}

#[test]
fn replay_buffer_is_bounded_by_cap() {
    let cap = 8usize;
    let pty = Pty::spawn(
        "/bin/sh",
        &["-c".into(), "sleep 0.2".into()],
        ".",
        80,
        24,
        cap,
    )
    .expect("spawn sh");
    // Push well past the cap; the ring must never exceed it.
    for _ in 0..10 {
        pty.push_replay(b"XXXX");
    }
    let snap = pty.snapshot_replay();
    assert!(
        snap.len() <= cap,
        "replay snapshot {} exceeded cap {cap}",
        snap.len()
    );
}
