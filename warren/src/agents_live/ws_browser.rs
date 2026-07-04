use crate::agents_live::AgentRegistry;
use crate::auth;
use crate::error::AppError;
use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use uuid::Uuid;

pub async fn ws_browser(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    if !auth::validate_admin_session(
        &state.db,
        &auth::read_session_cookie(&headers).unwrap_or_default(),
    )
    .await
    .unwrap_or(false)
    {
        return Err(AppError::Unauthorized);
    }
    let registry: AgentRegistry = state.live.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle(socket, registry, id).await {
            log::debug!("browser ws closed for agent {}: {e:?}", id);
        }
    }))
}

async fn handle(socket: WebSocket, registry: AgentRegistry, agent_id: Uuid) -> anyhow::Result<()> {
    let handle = registry
        .get(&agent_id)
        .ok_or_else(|| anyhow::anyhow!("agent not connected"))?
        .clone();
    let mut term_rx = handle.subscribe_term();
    let mut meta_rx = handle.subscribe_meta();

    let (mut sink, mut stream) = socket.split();

    for chunk in handle.replay_term() {
        let mut frame = vec![0x01u8];
        frame.extend_from_slice(&chunk);
        sink.send(Message::Binary(frame)).await?;
    }
    let resize_jiggle = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let snap = resize_jiggle.snapshot();
        let _ = resize_jiggle.restart(false).await;
        let _ = resize_jiggle
            .send_terminal_bytes(Bytes::from_static(b"\x1b[?25h"))
            .await;
        log::debug!(
            "resize jiggle for agent {} (state={:?})",
            agent_id,
            snap.state
        );
    });

    loop {
        tokio::select! {
            biased;
            chunk = term_rx.recv() => {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let mut frame = vec![0x01u8];
                frame.extend_from_slice(&bytes);
                if sink.send(Message::Binary(frame)).await.is_err() {
                    break;
                }
            }
            ev = meta_rx.recv() => {
                match ev {
                    Ok(body) => {
                        let env = crate::agents_live::wire::Envelope {
                            v: crate::agents_live::wire::PROTOCOL_VERSION,
                            seq: 0,
                            body,
                        };
                        if let Ok(s) = serde_json::to_string(&env) {
                            if sink.send(Message::Text(s)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = stream.next() => {
                let Some(msg) = msg else { break; };
                let msg = match msg {
                    Ok(m) => m,
                    Err(_) => break,
                };
                match msg {
                    Message::Text(t) => {
                        if let Ok(env) = serde_json::from_str::<crate::agents_live::wire::Envelope>(&t) {
                            if let Err(e) = forward_browser_message(&handle, env).await {
                                log::debug!("forward failed: {e:?}");
                            }
                        }
                    }
                    Message::Binary(mut b) => {
                        if b.is_empty() { continue; }
                        let chan = b.remove(0);
                        if chan == 0x01 {
                            handle.publish_term(Bytes::from(b));
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
        }
    }
    Ok(())
}

async fn forward_browser_message(
    handle: &crate::agents_live::AgentHandle,
    env: crate::agents_live::wire::Envelope,
) -> anyhow::Result<()> {
    use crate::agents_live::wire::EnvelopeBody;
    match env.body {
        EnvelopeBody::Prompt { text, .. } => {
            handle.prompt(&text, false).await?;
        }
        EnvelopeBody::Interrupt => handle.interrupt().await?,
        EnvelopeBody::Clear { hard } => handle.clear(hard).await?,
        EnvelopeBody::Resize { cols, rows } => {
            let _ = handle
                .send_terminal_bytes(Bytes::from(format!("\x1b[8;{rows};{cols}t").into_bytes()))
                .await;
        }
        EnvelopeBody::Repaint => {}
        EnvelopeBody::Restart { fresh } => handle.restart(fresh).await?,
        _ => {}
    }
    Ok(())
}
