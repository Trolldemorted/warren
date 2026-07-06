//! §D Milestone 5 — the `/agent/:id/shell` debug PTY (rabbit side).
//!
//! A second, optional PTY (`bash -i` by default) running on the same rabbit
//! as claude. It's a plain byte-pump: output is tagged with
//! [`TERM_CHAN_SHELL`] on the way to warren, and inbound shell-channel bytes
//! are written straight into the PTY. Enabled with `ENABLE_SHELL=1`.
//!
//! The shell has a much simpler lifecycle than claude — no crash-window
//! throttling, no session resume, no graceful-ESC dance, no trust dialog.
//! When it exits (the user typed `exit`, or bash died) we just respawn after
//! a short delay until the supervisor shuts down. That's why it doesn't share
//! claude's `spawn_run_one` machinery.
//!
//! Unlike the claude task, the reader and writer live on separate OS threads.
//! claude's TUI paints continuously, so its single-threaded read/try-write
//! loop never starves input for long; an idle `bash`, by contrast, produces
//! zero output, so a shared loop blocked in `read()` would never flush a typed
//! keystroke. Splitting them means a write never waits on a read.

use crate::config::Config;
use crate::link::LinkCmd;
use crate::pty::Pty;
use crate::wire::TERM_CHAN_SHELL;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Commands the supervisor sends to the shell PTY.
#[derive(Debug)]
pub enum ShellCmd {
    Write(Vec<u8>),
    #[allow(dead_code)]
    Resize { cols: u16, rows: u16 },
}

/// Supervisor-side handle to the shell task: a sender for inbound bytes.
pub struct ShellHandle {
    pub tx: mpsc::Sender<ShellCmd>,
}

/// How long to wait after the shell exits before respawning it, to avoid a
/// tight loop if the configured shell binary exits immediately (e.g. a bad
/// `SHELL_BIN`).
const RESPAWN_DELAY: Duration = Duration::from_millis(250);

/// Spawn the shell manager task. Returns a handle whose `tx` accepts inbound
/// bytes/resizes; output is pushed to `cmd_tx` tagged with `TERM_CHAN_SHELL`.
pub fn spawn(
    config: &Config,
    cmd_tx: mpsc::Sender<LinkCmd>,
    shutdown: Arc<AtomicBool>,
) -> ShellHandle {
    let (tx, rx) = mpsc::channel::<ShellCmd>(128);
    let bin = config.shell_bin.clone();
    let args = config.shell_args.clone();
    let workdir = config.workdir.clone();
    let cols = config.term_cols;
    let rows = config.term_rows;
    tokio::spawn(manage(
        bin, args, workdir, cols, rows, cmd_tx, rx, shutdown,
    ));
    ShellHandle { tx }
}

#[allow(clippy::too_many_arguments)]
async fn manage(
    bin: String,
    args: Vec<String>,
    workdir: String,
    cols: u16,
    rows: u16,
    cmd_tx: mpsc::Sender<LinkCmd>,
    mut rx: mpsc::Receiver<ShellCmd>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::SeqCst) {
        // Fresh per-generation std channel: the blocking PTY thread owns the
        // receiver; we forward async `ShellCmd`s into it while the generation
        // runs. Dropping `gen_tx` on respawn disconnects the old receiver.
        let (gen_tx, gen_rx) = std_mpsc::channel::<ShellCmd>();
        let bin_g = bin.clone();
        let args_g = args.clone();
        let workdir_g = workdir.clone();
        let cmd_tx_g = cmd_tx.clone();
        let shutdown_g = shutdown.clone();

        let mut gen = tokio::task::spawn_blocking(move || {
            run_generation(&bin_g, &args_g, &workdir_g, cols, rows, cmd_tx_g, gen_rx, shutdown_g)
        });

        loop {
            tokio::select! {
                biased;
                res = &mut gen => {
                    if let Ok(Err(e)) = res {
                        log::warn!("shell generation error: {e:?}");
                    }
                    break;
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(c) => {
                            // If the blocking thread already exited, its
                            // receiver is gone — the send fails harmlessly and
                            // the `gen` arm will fire next.
                            let _ = gen_tx.send(c);
                        }
                        None => {
                            // Supervisor dropped the sender; nothing more will
                            // ever arrive. Let the generation wind down on
                            // shutdown and stop managing.
                            let _ = gen.await;
                            return;
                        }
                    }
                }
            }
        }

        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(RESPAWN_DELAY).await;
    }
    log::info!("shell manager exiting");
}

/// One shell lifetime: spawn the PTY, pump output on a dedicated reader thread,
/// and service writes/resizes + exit detection on this thread. Returns when the
/// child exits or shutdown is requested.
#[allow(clippy::too_many_arguments)]
fn run_generation(
    bin: &str,
    args: &[String],
    workdir: &str,
    cols: u16,
    rows: u16,
    cmd_tx: mpsc::Sender<LinkCmd>,
    gen_rx: std_mpsc::Receiver<ShellCmd>,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    // Shell needs no replay buffer of its own (warren keeps the bounded term
    // ring); pass a zero cap.
    let mut pty = Pty::spawn(bin, args, workdir, cols, rows, 0)?;
    let mut reader = pty.reader();
    let mut writer = pty.writer();
    log::info!("shell pty spawned: bin={bin} args={args:?}");

    // Reader thread: blocking-read PTY output → tagged binary frames to warren.
    // §A.7: each shell generation owns its own per-channel seq counter,
    // independent of claude's. Single-producer (this thread), so
    // `Ordering::Relaxed`-equivalent reasoning is fine — just a plain
    // u64. Starts at 1, bumped before assignment.
    let reader_join = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut next_seq: u64 = 1;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let seq = next_seq;
                    next_seq = next_seq.wrapping_add(1);
                    if cmd_tx
                        .blocking_send(LinkCmd::SendBinary {
                            chan: TERM_CHAN_SHELL,
                            seq,
                            data: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // Manager loop: writes + resize + exit/shutdown detection. Poll the write
    // channel with a timeout so an idle shell (no writes) still checks liveness.
    loop {
        if shutdown.load(Ordering::SeqCst) {
            let _ = pty.terminate();
            break;
        }
        match gen_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ShellCmd::Write(b)) => {
                if writer.write_all(&b).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            Ok(ShellCmd::Resize { cols, rows }) => {
                let _ = pty.resize(cols, rows);
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                if !pty.alive() {
                    break;
                }
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = pty.terminate();
    let _ = pty.wait();
    // Closing the PTY makes the reader's next read return 0/Err, so the reader
    // thread exits; join it so we don't leak threads across respawns.
    let _ = reader_join.join();
    log::info!("shell pty exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one shell generation against `/bin/cat` (a trivial stdin→stdout
    /// copy): a write must round-trip back out tagged with `TERM_CHAN_SHELL`,
    /// never the claude channel. This pins the two things the supervisor
    /// relies on — the shell owns its own channel byte, and inbound writes
    /// actually reach the PTY.
    #[tokio::test]
    async fn generation_round_trips_writes_on_shell_channel() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<LinkCmd>(64);
        let (gen_tx, gen_rx) = std_mpsc::channel::<ShellCmd>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_g = shutdown.clone();
        let gen = tokio::task::spawn_blocking(move || {
            run_generation(
                "/bin/cat",
                &[],
                ".",
                80,
                24,
                cmd_tx,
                gen_rx,
                shutdown_g,
            )
        });

        gen_tx.send(ShellCmd::Write(b"ping\n".to_vec())).unwrap();

        // Collect output until we see the echoed marker or time out.
        let mut seen = Vec::new();
        let mut first_seq: Option<u64> = None;
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(cmd) = cmd_rx.recv().await {
                if let LinkCmd::SendBinary { chan, seq, data } = cmd {
                    assert_eq!(chan, TERM_CHAN_SHELL, "shell output must carry the shell channel");
                    // §A.7: pin the first chunk's seq to 1 — the
                    // counter starts at 1, and increment-before-assign
                    // means the first read off the wire gets seq=1.
                    if first_seq.is_none() {
                        first_seq = Some(seq);
                        assert_eq!(
                            seq, 1,
                            "first shell-channel byte must carry seq=1 (counter starts at 1)"
                        );
                    }
                    seen.extend_from_slice(&data);
                    if seen.windows(4).any(|w| w == b"ping") {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .expect("timed out waiting for shell echo");
        assert!(found, "expected the written bytes to echo back from the pty");
        assert_eq!(first_seq, Some(1), "first shell read must carry seq=1");

        shutdown.store(true, Ordering::SeqCst);
        let _ = gen.await;
    }
}

