use crate::config::Config;
use crate::health::{serve as serve_health, HealthState};
use crate::input;
use crate::link::{Link, LinkCmd, LinkEvent, ReplaySnapFn};
use crate::observer::hooks::{ObserverEvent, ObserverHandle};
use crate::observer::state::State;
use crate::observer::transcript::{default_transcript_path, TranscriptTail, UsageUpdate};
use crate::pty::Pty;
use crate::wire::{
    Envelope, EnvelopeBody, LogLine, StateFrame, TermSize, TurnDone, UsageSnapshot,
    PROTOCOL_VERSION,
};
use anyhow::Result;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn run(config: Config) -> Result<()> {
    std::fs::create_dir_all(&config.workdir).ok();

    let health = HealthState::new();
    {
        let cfg = config.clone();
        let h = health.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_health(cfg.health_port, h).await {
                log::error!("health server stopped: {e:?}");
            }
        });
    }

    let observer = ObserverHandle::new();
    {
        let obs = observer.clone();
        let port = config.observer_port;
        tokio::spawn(async move {
            if let Err(e) = crate::observer::hooks::serve(port, obs).await {
                log::error!("observer server stopped: {e:?}");
            }
        });
    }

    let agent_id = Uuid::new_v4();
    let claude_version = detect_claude_version(&config).await;

    loop {
        if let Err(e) = run_one(&config, &health, agent_id, &claude_version, &observer).await {
            log::error!("rabbit session ended with error: {e:?}");
        } else {
            log::info!("rabbit session ended cleanly");
        }
        log::info!("rabbit supervisor sleeping 5s before restart");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn detect_claude_version(config: &Config) -> String {
    let mut cmd = Command::new(&config.claude_bin);
    for a in &config.claude_args {
        cmd.arg(a);
    }
    cmd.arg("--version");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    match tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

#[derive(Debug)]
pub enum PtyCmd {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Terminate,
}

#[derive(Debug)]
pub enum PtyEvt {
    Read(Vec<u8>),
    Exited(()),
}

async fn run_one(
    config: &Config,
    health: &HealthState,
    agent_id: Uuid,
    claude_version: &str,
    observer: &ObserverHandle,
) -> Result<()> {
    log::info!(
        "spawning pty: bin={} args={:?} workdir={}",
        config.claude_bin,
        config.claude_args,
        config.workdir
    );

    let (pty_tx, mut pty_rx) = mpsc::channel::<PtyCmd>(128);
    let (pty_evt_tx, mut pty_evt_rx) = mpsc::channel::<PtyEvt>(128);

    // Bounded ring of recent PTY output, shared between the supervisor's
    // main loop (writer) and the link's snapshot closure (reader). link.attempt
    // calls the closure on every reconnect so a fresh snapshot reaches warren,
    // not a stale one captured at spawn.
    let replay_buf: Arc<Mutex<VecDeque<Vec<u8>>>> =
        Arc::new(Mutex::new(VecDeque::new()));
    let replay_cap = config.replay_bytes;

    let bin = config.claude_bin.clone();
    let args = config.claude_args.clone();
    let workdir = config.workdir.clone();
    let cols = config.term_cols;
    let rows = config.term_rows;
    let replay = config.replay_bytes;

    let pty_join = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut pty = Pty::spawn(&bin, &args, &workdir, cols, rows, replay)?;
        let mut reader = pty.reader();
        let mut writer = pty.writer();
        let initial_replay = pty.snapshot_replay().to_vec();

        let mut io_buf = [0u8; 4096];
        loop {
            if let Ok(cmd) = pty_rx.try_recv() {
                match cmd {
                    PtyCmd::Write(b) => {
                        use std::io::Write;
                        if writer.write_all(&b).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    PtyCmd::Resize { cols, rows } => {
                        let _ = pty.resize(cols, rows);
                    }
                    PtyCmd::Terminate => {
                        let _ = pty.terminate();
                    }
                }
                continue;
            }
            use std::io::Read;
            match reader.read(&mut io_buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty_evt_tx
                        .blocking_send(PtyEvt::Read(io_buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
            if !pty.alive() {
                let _ = pty_evt_tx.blocking_send(PtyEvt::Exited(()));
                break;
            }
        }
        Ok(initial_replay)
    });

    let pty_task = tokio::spawn(async move {
        match pty_join.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log::error!("pty task error: {e:?}"),
            Err(e) => log::error!("pty task join error: {e:?}"),
        }
    });

    match pty_evt_rx.recv().await {
        Some(PtyEvt::Read(_)) => {
            // Seed the shared replay buffer from the initial burst of PTY
            // output. The link's snapshot closure reads from this same
            // buffer on every (re)connect, so we don't need to capture the
            // bytes separately here.
            let mut buf = replay_buf.lock();
            while let Ok(PtyEvt::Read(chunk)) = pty_evt_rx.try_recv() {
                buf.push_back(chunk);
            }
            trim_replay(&mut buf, replay_cap);
        }
        Some(PtyEvt::Exited(_)) | None => {
            log::error!("pty exited before producing replay");
            return Ok(());
        }
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<LinkCmd>(128);
    let (event_tx, mut event_rx) = mpsc::channel::<LinkEvent>(128);

    // Snapshot closure for the link: concatenates the current contents of
    // the replay buffer (the most recent `replay_cap` bytes of PTY output).
    // Called by link.attempt on every (re)connect.
    let snap_buf = replay_buf.clone();
    let replay_snap: ReplaySnapFn = Arc::new(move || {
        let buf = snap_buf.lock();
        let mut out = Vec::new();
        for chunk in buf.iter() {
            out.extend_from_slice(chunk);
        }
        out
    });

    let link = Link::new(
        config.warren_url.clone(),
        config.agent_token.clone(),
        agent_id,
        claude_version.to_string(),
        TermSize {
            cols: config.term_cols,
            rows: config.term_rows,
        },
        cmd_rx,
        event_tx,
        replay_snap,
    );
    let _link_handle = tokio::spawn(async move {
        if let Err(e) = link.run().await {
            log::error!("link exited: {e:?}");
        }
    });

    let _transcript_task = {
        let path = default_transcript_path(std::path::Path::new(&config.workdir));
        let (utx, mut urx) = mpsc::channel::<UsageUpdate>(64);
        let tail = TranscriptTail::new(path);
        tokio::spawn(async move {
            if let Err(e) = tail.run(utx, 250).await {
                log::warn!("transcript tail stopped: {e:?}");
            }
        });
        let tx_clone = cmd_tx.clone();
        tokio::spawn(async move {
            while let Some(update) = urx.recv().await {
                let env = Envelope {
                    v: PROTOCOL_VERSION,
                    seq: 0,
                    body: EnvelopeBody::Usage(update.usage),
                };
                if let Ok(s) = serde_json::to_string(&env) {
                    let _ = tx_clone.send(LinkCmd::SendTextRaw(s)).await;
                }
            }
        })
    };

    health.set_alive(true);
    let _ = send_state(
        &cmd_tx,
        StateFrame {
            state: "idle".into(),
            session_id: None,
            reason: None,
        },
        Some(State::Idle),
    )
    .await;

    let mut obs_rx = observer.tx.subscribe();
    let mut alive = true;
    let mut out_buf: Vec<u8> = Vec::with_capacity(4096);

    while alive {
        tokio::select! {
            biased;
            chunk = pty_evt_rx.recv() => {
                match chunk {
                    Some(PtyEvt::Read(c)) => {
                        {
                            let mut buf = replay_buf.lock();
                            buf.push_back(c.clone());
                            trim_replay(&mut buf, replay_cap);
                        }
                        let _ = cmd_tx.send(LinkCmd::SendBinary(c)).await;
                    }
                    Some(PtyEvt::Exited(_)) => alive = false,
                    None => alive = false,
                }
            }
            ev = event_rx.recv() => {
                match ev {
                    Some(LinkEvent::Text(env)) => {
                        out_buf.clear();
                        handle_warren_command(&env, &mut out_buf, &pty_tx).await;
                        if !out_buf.is_empty() {
                            let _ = pty_tx.send(PtyCmd::Write(out_buf.clone())).await;
                        }
                    }
                    Some(LinkEvent::Binary(b)) => {
                        let _ = pty_tx.send(PtyCmd::Write(b)).await;
                    }
                    None => break,
                }
            }
            evt = obs_rx.recv() => {
                if let Ok(ev) = evt {
                    forward_observer_event(&cmd_tx, &ev).await;
                }
            }
        }
    }

    health.set_alive(false);
    let _ = pty_tx.send(PtyCmd::Terminate).await;
    let _ = cmd_tx.send(LinkCmd::Shutdown).await;
    let _ = pty_task.await;
    Ok(())
}

async fn forward_observer_event(cmd_tx: &mpsc::Sender<LinkCmd>, ev: &ObserverEvent) {
    let env = build_envelope(ev);
    let body = match env {
        Some(e) => e,
        None => return,
    };
    let wrapper = Envelope {
        v: PROTOCOL_VERSION,
        seq: 0,
        body: body.clone(),
    };
    if let Ok(s) = serde_json::to_string(&wrapper) {
        let _ = cmd_tx.send(LinkCmd::SendTextRaw(s)).await;
    }
}

fn build_envelope(ev: &ObserverEvent) -> Option<EnvelopeBody> {
    let raw = match ev.kind {
        "session" => EnvelopeBody::Session(crate::wire::SessionInfo {
            session_id: ev.session_id.clone().unwrap_or_default(),
            resumed: false,
        }),
        "session_end" => EnvelopeBody::State(StateFrame {
            state: "ended".into(),
            session_id: ev.session_id.clone(),
            reason: Some("session_end".into()),
        }),
        "prompt_echo" => EnvelopeBody::PromptEcho(crate::wire::PromptEcho {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            text: ev
                .raw
                .as_ref()
                .and_then(|r| r.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            by: "admin".to_string(),
        }),
        "stop_hook" => EnvelopeBody::StopHook {
            prompt_id: ev.prompt_id.unwrap_or_else(Uuid::nil),
            usage: ev.usage.clone(),
            error: None,
        },
        "log" => EnvelopeBody::Log(LogLine {
            level: "info".to_string(),
            message: ev.raw.as_ref().map(|r| r.to_string()).unwrap_or_default(),
        }),
        _ => return None,
    };
    Some(raw)
}

async fn handle_warren_command(env: &Envelope, out: &mut Vec<u8>, pty_tx: &mpsc::Sender<PtyCmd>) {
    let mut shim = BufShim { out };
    match &env.body {
        EnvelopeBody::Prompt { text, .. } => {
            input::paste(&mut shim, text).ok();
        }
        EnvelopeBody::Slash { cmd } => {
            input::slash(&mut shim, cmd).ok();
        }
        EnvelopeBody::Interrupt => {
            input::interrupt(&mut shim).ok();
        }
        EnvelopeBody::Clear { hard: _ } => {
            input::slash(&mut shim, "clear").ok();
        }
        EnvelopeBody::Restart { .. } => {
            let _ = pty_tx.send(PtyCmd::Terminate).await;
        }
        EnvelopeBody::Resize { cols, rows } => {
            let _ = pty_tx
                .send(PtyCmd::Resize {
                    cols: *cols,
                    rows: *rows,
                })
                .await;
        }
        EnvelopeBody::Repaint => {}
        _ => {}
    }
}

pub async fn send_state(
    cmd_tx: &mpsc::Sender<LinkCmd>,
    frame: StateFrame,
    _state: Option<State>,
) -> Result<()> {
    let env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 0,
        body: EnvelopeBody::State(frame),
    };
    if let Ok(s) = serde_json::to_string(&env) {
        let _ = cmd_tx.send(LinkCmd::SendTextRaw(s)).await;
    }
    Ok(())
}

#[allow(dead_code)]
pub fn _unused_usage() -> UsageSnapshot {
    UsageSnapshot::default()
}

#[allow(dead_code)]
pub fn _unused_turn() -> TurnDone {
    TurnDone {
        prompt_id: Uuid::nil(),
        started_at: chrono::Utc::now(),
        ended_at: chrono::Utc::now(),
        usage: None,
        error: None,
    }
}

struct BufShim<'a> {
    out: &'a mut Vec<u8>,
}

/// Drop oldest chunks from `buf` until its total byte length is `<= cap`.
fn trim_replay(buf: &mut VecDeque<Vec<u8>>, cap: usize) {
    let mut total: usize = buf.iter().map(|v| v.len()).sum();
    while total > cap {
        match buf.pop_front() {
            Some(front) => total -= front.len(),
            None => break,
        }
    }
}

impl<'a> std::io::Write for BufShim<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
