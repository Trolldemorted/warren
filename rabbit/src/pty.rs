use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::Arc;

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

    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn terminate(&mut self) -> Result<()> {
        self.child.kill().context("killing child")?;
        Ok(())
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
