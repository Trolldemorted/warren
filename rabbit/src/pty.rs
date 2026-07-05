use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

pub type PtyExitStatus = ExitStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Clean,
    Crashed,
}

impl ExitKind {
    pub fn from(status: &PtyExitStatus) -> Self {
        if status.success() {
            Self::Clean
        } else {
            Self::Crashed
        }
    }
}

pub struct Pty {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub replay: Arc<Mutex<VecDeque<u8>>>,
    #[allow(dead_code)]
    pub replay_cap: usize,
    #[allow(dead_code)]
    pub cols: u16,
    #[allow(dead_code)]
    pub rows: u16,
}

impl Pty {
    pub fn spawn(
        bin: &str,
        args: &[String],
        workdir: &str,
        cols: u16,
        rows: u16,
        replay_cap: usize,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening pty")?;
        let mut cmd = CommandBuilder::new(bin);
        for a in args {
            cmd.arg(a);
        }
        cmd.cwd(workdir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(cmd).context("spawning child")?;
        drop(pair.slave);
        Ok(Self {
            master: pair.master,
            child,
            replay: Arc::new(Mutex::new(VecDeque::with_capacity(replay_cap))),
            replay_cap,
            cols,
            rows,
        })
    }

    pub fn reader(&self) -> Box<dyn Read + Send> {
        self.master.try_clone_reader().expect("cloning pty reader")
    }

    pub fn writer(&self) -> Box<dyn Write + Send> {
        self.master.take_writer().expect("taking pty writer")
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resizing pty")?;
        Ok(())
    }

    /// Nudge the kernel winsize so a SIGWINCH reaches the child. The kernel
    /// only emits SIGWINCH when the winsize actually changes, so calling
    /// `resize` with the same dimensions twice is a no-op. To force the
    /// child's TUI to redraw (e.g. after a late-join replay buffer landed
    /// in a fresh xterm.js), we go +1 column, settle, then back to the
    /// original. Two SIGWINCHs; one full repaint.
    pub fn jiggle(&self, cols: u16, rows: u16) -> Result<()> {
        self.resize(cols.saturating_add(1), rows)
            .context("jiggle widen")?;
        std::thread::sleep(Duration::from_millis(50));
        self.resize(cols, rows).context("jiggle restore")?;
        Ok(())
    }

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn terminate(&mut self) -> Result<()> {
        self.child.kill().context("killing child")?;
        Ok(())
    }

    /// Block until the child exits, returning the captured exit status.
    pub fn wait(&mut self) -> Result<PtyExitStatus> {
        self.child.wait().context("waiting on child")
    }

    #[allow(dead_code)]
    pub fn push_replay(&self, chunk: &[u8]) {
        let mut g = self.replay.lock();
        for b in chunk {
            g.push_back(*b);
        }
        while g.len() > self.replay_cap {
            g.pop_front();
        }
    }

    pub fn snapshot_replay(&self) -> Bytes {
        let g = self.replay.lock();
        BytesMut::from(g.as_slices().0).freeze()
    }

    #[allow(dead_code)]
    pub fn coalesced_replay(&self) -> Vec<Bytes> {
        let g = self.replay.lock();
        if g.is_empty() {
            return Vec::new();
        }
        vec![Bytes::copy_from_slice(g.as_slices().0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_kind_clean_for_zero_code() {
        let s = PtyExitStatus::with_exit_code(0);
        assert_eq!(ExitKind::from(&s), ExitKind::Clean);
    }

    #[test]
    fn exit_kind_crashed_for_signal() {
        let s = PtyExitStatus::with_signal("SIGKILL");
        assert_eq!(ExitKind::from(&s), ExitKind::Crashed);
    }

    #[test]
    fn exit_kind_crashed_for_nonzero_code() {
        let s = PtyExitStatus::with_exit_code(2);
        assert_eq!(ExitKind::from(&s), ExitKind::Crashed);
    }

    /// jiggle returns the kernel winsize to the requested (cols, rows) after
    /// the +1/settle/restore sequence. We can't directly observe SIGWINCH
    /// delivery from the test process, but the post-condition is observable
    /// via the master PTY's winsize, and that's the thing the caller cares
    /// about anyway.
    #[test]
    fn jiggle_restores_original_size() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_string(), "sleep 0.5".to_string()],
            ".",
            120,
            40,
            4096,
        )
        .expect("spawn sh");
        let original = pty.master.get_size().expect("get_size");
        assert_eq!(original.cols, 120);
        assert_eq!(original.rows, 40);
        pty.jiggle(120, 40).expect("jiggle");
        let after = pty.master.get_size().expect("get_size after jiggle");
        assert_eq!(after.cols, 120, "jiggle must end at original cols");
        assert_eq!(after.rows, 40, "jiggle must end at original rows");
        let _ = pty.terminate();
        let _ = pty.wait();
    }

    #[test]
    fn jiggle_handles_max_cols_without_overflow() {
        // saturating_add(1) at u16::MAX must stay at u16::MAX rather than wrap
        // to zero (which would be a giant resize and probably fail ioctl).
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_string(), "sleep 0.5".to_string()],
            ".",
            u16::MAX,
            24,
            4096,
        )
        .expect("spawn sh");
        // Some platforms reject u16::MAX as a column count; ignore resize
        // errors here — what we're asserting is the math, not the ioctl.
        let _ = pty.jiggle(u16::MAX, 24);
        let _ = pty.terminate();
        let _ = pty.wait();
    }
}
