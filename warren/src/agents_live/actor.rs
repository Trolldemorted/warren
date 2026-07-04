use crate::agents_live::handle::AgentStateSnapshot;
use crate::agents_live::wire::{
    Envelope, EnvelopeBody, HelloDown, UsageSnapshot, PROTOCOL_VERSION,
};
use crate::agents_live::AgentHandle;
use crate::db::Db;
use crate::db_ops;
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

pub enum Command {
    Prompt {
        id: Uuid,
        text: String,
        by: String,
        wait: bool,
        reply: Option<oneshot::Sender<TurnOutcomeMsg>>,
    },
    Clear {
        hard: bool,
        reply: Option<oneshot::Sender<()>>,
    },
    Compact,
    Interrupt,
    Restart {
        fresh: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    SendKeys(Bytes),
}

#[derive(Debug, Clone)]
pub struct TurnOutcomeMsg {
    pub prompt_id: Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub usage: Option<UsageSnapshot>,
    pub error: Option<String>,
}

#[allow(dead_code)]
pub struct ActorHandle {
    pub cmd_tx: mpsc::Sender<Command>,
    pub join: tokio::task::JoinHandle<()>,
}

pub async fn run(
    db: Db,
    handle: AgentHandle,
    agent_id: Uuid,
    socket: WebSocket,
    cmd_rx: mpsc::Receiver<Command>,
) -> Result<()> {
    let join = tokio::spawn(run_inner(db, handle, agent_id, socket, cmd_rx));
    join.await.map_err(|e| anyhow::anyhow!("actor join: {e}"))?;
    Ok(())
}

async fn run_inner(
    db: Db,
    handle: AgentHandle,
    agent_id: Uuid,
    socket: WebSocket,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    let (mut sink, mut stream) = socket.split();

    let hello = match read_hello(&mut stream).await {
        Ok(h) => h,
        Err(e) => {
            log::warn!("actor hello read failed: {e:?}");
            return;
        }
    };

    // Resume seq past the highest row we already persisted for this agent.
    // Hello takes the first free seq; subsequent messages advance from there.
    // Without this, every reconnect would try to insert seq=1 again and
    // violate the (agent_id, seq) unique index.
    let mut seq: i64 = match db_ops::next_event_seq(&db, agent_id).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("next_event_seq failed for {agent_id}: {e:?}");
            return;
        }
    };
    // Persist the hello BEFORE publishing the new state to subscribers.
    // The DB row is the source of truth for "what happened"; the meta
    // broadcast is local and best-effort. If the insert fails we'd rather
    // have no row AND no broadcast than a row-less broadcast that misleads
    // SSE listeners.
    persist_event(
        &db,
        agent_id,
        &serde_json::to_value(&hello).unwrap_or(serde_json::Value::Null),
        "hello",
        seq,
    )
    .await
    .ok();
    seq += 1;

    handle.update_state(AgentStateSnapshot {
        state: hello.state,
        session_id: hello.session_id.clone(),
        claude_version: Some(hello.claude_version.clone()),
        last_usage: UsageSnapshot {
            source: "transcript".to_string(),
            ..Default::default()
        },
    });

    let mut pending: std::collections::VecDeque<(Uuid, oneshot::Sender<TurnOutcomeMsg>)> =
        std::collections::VecDeque::new();
    let mut started_at: HashMap<Uuid, chrono::DateTime<Utc>> = HashMap::new();
    let mut last_usage = UsageSnapshot {
        source: "transcript".to_string(),
        ..Default::default()
    };

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                if let Err(e) = dispatch(cmd, &mut sink, &mut pending, &mut started_at).await {
                    log::warn!("dispatch error: {e:?}");
                }
            }
            msg = stream.next() => {
                let Some(msg) = msg else { break; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("ws recv error: {e:?}");
                        break;
                    }
                };
                match msg {
                    Message::Text(t) => {
                        let env: Envelope = match serde_json::from_str(&t) {
                            Ok(v) => v,
                            Err(e) => {
                                log::warn!("bad envelope from rabbit: {e:?}");
                                continue;
                            }
                        };
                        if let EnvelopeBody::State(s) = &env.body {
                            handle.update_state(AgentStateSnapshot {
                                state: s.state,
                                session_id: s.session_id.clone(),
                                claude_version: None,
                                last_usage: last_usage.clone(),
                            });
                        }
                        if let EnvelopeBody::PromptEcho(pe) = &env.body {
                            started_at.insert(pe.prompt_id, Utc::now());
                        }
                        if let EnvelopeBody::StopHook { prompt_id, usage, error } = &env.body {
                            let actual_id = pending
                                .front()
                                .map(|(id, _)| *id)
                                .unwrap_or(*prompt_id);
                            let outcome = TurnOutcomeMsg {
                                prompt_id: actual_id,
                                started_at: started_at.remove(&actual_id).unwrap_or_else(Utc::now),
                                ended_at: Utc::now(),
                                usage: usage.clone(),
                                error: error.clone(),
                            };
                            if let Some(u) = usage {
                                last_usage = u.clone();
                            }
                            if let Some((_, tx)) = pending.pop_front() {
                                let _ = tx.send(outcome);
                            }
                        }
                        let payload_json = serde_json::to_value(&env).unwrap_or(serde_json::Value::Null);
                        let kind = envelope_kind(&env.body).to_string();
                        if !matches!(&env.body, EnvelopeBody::Ack { .. }) {
                            persist_event(&db, agent_id, &payload_json, &kind, seq).await.ok();
                            seq += 1;
                        }
                        handle.publish_meta(env.body);
                    }
                    Message::Binary(mut b) => {
                        if b.is_empty() { continue; }
                        let _chan = b.remove(0);
                        handle.publish_term(Bytes::from(b));
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
        }
    }
}

async fn read_hello(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<HelloDown> {
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        if let Message::Text(t) = msg {
            let env: Envelope = serde_json::from_str(&t)?;
            if env.v != PROTOCOL_VERSION {
                anyhow::bail!("protocol mismatch: {}", env.v);
            }
            if let EnvelopeBody::Hello(h) = env.body {
                return Ok(h);
            }
        }
    }
    anyhow::bail!("no hello from rabbit")
}

async fn dispatch(
    cmd: Command,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    pending: &mut std::collections::VecDeque<(Uuid, oneshot::Sender<TurnOutcomeMsg>)>,
    started_at: &mut HashMap<Uuid, chrono::DateTime<Utc>>,
) -> Result<()> {
    match cmd {
        Command::Prompt {
            id,
            text,
            by,
            wait,
            reply,
        } => {
            let started = Utc::now();
            started_at.insert(id, started);
            if wait {
                if let Some(tx) = reply {
                    pending.push_back((id, tx));
                }
            }
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Prompt { id, text, by },
            };
            sink.send(Message::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Clear { hard, reply } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Clear { hard },
            };
            sink.send(Message::Text(serde_json::to_string(&env)?))
                .await?;
            if let Some(tx) = reply {
                let _ = tx.send(());
            }
        }
        Command::Compact => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Slash {
                    cmd: "compact".to_string(),
                },
            };
            sink.send(Message::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Interrupt => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Interrupt,
            };
            sink.send(Message::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Restart { fresh } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Restart { fresh },
            };
            sink.send(Message::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::Resize { cols, rows } => {
            let env = Envelope {
                v: PROTOCOL_VERSION,
                seq: 0,
                body: EnvelopeBody::Resize { cols, rows },
            };
            sink.send(Message::Text(serde_json::to_string(&env)?))
                .await?;
        }
        Command::SendKeys(b) => {
            let mut frame = vec![crate::agents_live::wire::TERM_CHAN_CLAUDE];
            frame.extend_from_slice(&b);
            sink.send(Message::Binary(frame)).await?;
        }
    }
    Ok(())
}

fn envelope_kind(body: &EnvelopeBody) -> &'static str {
    match body {
        EnvelopeBody::Hello(_) => "hello",
        EnvelopeBody::Ack { .. } => "ack",
        EnvelopeBody::State(_) => "state",
        EnvelopeBody::PromptEcho(_) => "prompt_echo",
        EnvelopeBody::TurnDone(_) => "turn_done",
        EnvelopeBody::Usage(_) => "usage",
        EnvelopeBody::Cleared { .. } => "cleared",
        EnvelopeBody::Session(_) => "session",
        EnvelopeBody::TranscriptMsg { .. } => "transcript_msg",
        EnvelopeBody::Log(_) => "log",
        EnvelopeBody::Pong => "pong",
        EnvelopeBody::Prompt { .. } => "prompt",
        EnvelopeBody::Slash { .. } => "slash",
        EnvelopeBody::Interrupt => "interrupt",
        EnvelopeBody::Clear { .. } => "clear",
        EnvelopeBody::Restart { .. } => "restart",
        EnvelopeBody::Resize { .. } => "resize",
        EnvelopeBody::Repaint => "repaint",
        EnvelopeBody::StopHook { .. } => "stop_hook",
    }
}

async fn persist_event(
    db: &Db,
    agent_id: Uuid,
    payload: &serde_json::Value,
    kind: &str,
    seq: i64,
) -> Result<()> {
    let id = Uuid::new_v4();
    db_ops::insert_agent_event(db, id, agent_id, seq, kind, payload.clone()).await?;
    Ok(())
}
