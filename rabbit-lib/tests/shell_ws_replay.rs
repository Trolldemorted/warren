//! §D shell WS regression — the page must see buffered shell bytes
//! on connect, and the server must fire a `ShellRepaint` after the
//! replay drain so bash redraws its prompt for a fresh browser pane.
//!
//! Spins up a tokio WS pair, registers an `AgentHandle`, calls
//! `ws_shell::handle` on the server side, and pins three contracts:
//! 1. Frames already in `term_ring` (mimicking the bash -i prompt that
//!    arrived before the browser connected) ARE delivered to the
//!    browser as the first messages.
//! 2. Frames published AFTER the WS opens (mimicking live shell bytes)
//!    are also delivered.
//! 3. After the replay drain, `ws_shell::handle` enqueues a
//!    `Command::ShellRepaint` on the actor's `cmd_tx` so the rabbit
//!    SIGWINCH-jiggles the shell PTY (the contract that closes the
//!    "missing shell prompt" gap).
//!
//! These are the bytes the browser pane relies on to render the
//! current shell state. Without them the pane opens to a blank screen.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rabbit_lib::server::handle::Command;
use rabbit_lib::server::handle::{AgentHandle, AgentStateSnapshot};
use rabbit_lib::server::registry::new_registry;
use rabbit_lib::server::ws_shell::handle as ws_shell_handle;
use rabbit_lib::server::WsTransport;
use rabbit_lib::wire::{
    Envelope, EnvelopeBody, Key, SendKey, TermFrame, TermSize, PROTOCOL_VERSION, TERM_CHAN_SHELL,
};

#[tokio::test]
async fn shell_prompt_is_delivered_to_browser_ws_on_open() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let registry = new_registry();
    let agent_id = uuid::Uuid::new_v4();
    // Build the handle FIRST so we can install a known cmd_tx —
    // this keeps the existing capture-replay contract while also
    // letting the repaint-firing contract (test #2 below) observe
    // `ShellRepaint` on the same cmd_rx. Then register the
    // pre-built handle into the registry.
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel::<Command>(8);
    let handle = AgentHandle::with_cmd_tx(agent_id, cmd_tx);
    registry.entry(agent_id).or_insert_with(|| handle.clone());
    // §A.7: register the handle BEFORE the browser WS opens so the
    // first message we publish is in the term_ring when ws_shell::handle
    // calls `handle.replay_term()`.
    let prompt_bytes: Vec<u8> = b"\x1b[?2004hroot@dev-warren:/tmp# ".to_vec();
    handle.publish_term(TermFrame {
        chan: TERM_CHAN_SHELL,
        seq: 1,
        data: prompt_bytes.clone(),
    });
    // Simulate the boundary: now the browser WS opens.

    let server_registry = registry.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let transport = TungsteniteTransport::new(ws);
        let _ = ws_shell_handle(transport, server_registry, agent_id, false).await;
    });

    // Client side: connect, read until we see the prompt.
    let connect = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut client = tokio_tungstenite::client_async("ws://localhost/socket", connect)
        .await
        .unwrap()
        .0;

    let first = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("first frame within 2s")
        .expect("first frame not None")
        .expect("first frame ok");
    let bytes = match first {
        tokio_tungstenite::tungstenite::Message::Binary(b) => b,
        other => panic!("expected Binary, got {other:?}"),
    };
    assert!(
        bytes.len() >= 10,
        "frame must include <chan:1><seq:8> prelude, got {} bytes",
        bytes.len()
    );
    assert_eq!(
        bytes[0], TERM_CHAN_SHELL,
        "shell-channel frame must carry TERM_CHAN_SHELL (0x02), got 0x{:02x}",
        bytes[0]
    );
    let payload = bytes[9..].to_vec();
    assert_eq!(
        payload, prompt_bytes,
        "browser should see exactly the bash prompt bytes that were in term_ring when WS opened"
    );

    // Now publish a live shell byte and confirm it streams through.
    handle.publish_term(TermFrame {
        chan: TERM_CHAN_SHELL,
        seq: 2,
        data: b"live-shell-byte".to_vec(),
    });
    let second = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("live frame within 2s")
        .expect("live frame not None")
        .expect("live frame ok");
    let live_bytes = match second {
        tokio_tungstenite::tungstenite::Message::Binary(b) => b,
        other => panic!("expected Binary live, got {other:?}"),
    };
    assert_eq!(live_bytes[9..].to_vec(), b"live-shell-byte".to_vec());

    client.close(None).await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server_task).await;
}

/// §D shell WS late-join repaint: after the replay buffer flushes,
/// `ws_shell::handle` MUST enqueue `Command::ShellRepaint` on the
/// actor's cmd_tx so the rabbit SIGWINCH-jiggles the shell PTY.
/// Without this the bash prompt can be missing on page load when
/// the ring has rolled over or bash hasn't yet emitted it.
///
/// Pinning this contract at the WS handler layer (rather than only at
/// `AgentHandle::shell_repaint`) guarantees the WS handler still
/// fires it after a future refactor — e.g. if someone moves the
/// repaint into a per-handle callback, this test would catch a
/// regression that left the WS handler calling the wrong method.
#[tokio::test]
async fn ws_shell_handle_fires_shell_repaint_after_replay() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let registry = new_registry();
    let agent_id = uuid::Uuid::new_v4();
    // Install a known cmd_tx on the handle — `shell_repaint` will
    // send through this sender.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(8);
    let handle = AgentHandle::with_cmd_tx(agent_id, cmd_tx);
    // Seed the cached term_size so we can assert the repaint
    // payload carries the correct dimensions. The default AgentState
    // has `term_size: None` and ws_shell::handle falls back to
    // (160, 50) in that case — assert THAT here too.
    let handle_for_size = handle.clone();
    handle_for_size.update_state(AgentStateSnapshot {
        term_size: Some(TermSize {
            cols: 200,
            rows: 60,
        }),
        ..AgentStateSnapshot::default()
    });
    registry.entry(agent_id).or_insert_with(|| handle.clone());

    let server_registry = registry.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let transport = TungsteniteTransport::new(ws);
        let _ = ws_shell_handle(transport, server_registry, agent_id, false).await;
    });

    // Connect a client — the WS handler will start its loop and
    // fire `shell_repaint` on `tokio::spawn`. We don't need to
    // read any frames; the cmd_tx capture is the contract.
    let connect = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut client = tokio_tungstenite::client_async("ws://localhost/socket", connect)
        .await
        .unwrap()
        .0;

    // Give the WS handler a beat to enter its loop, drain the
    // (empty) replay, and spawn the repaint task.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Drain commands until we see ShellRepaint or time out.
    let cmd = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match cmd_rx.recv().await {
                Some(Command::ShellRepaint { cols, rows }) => return Some((cols, rows)),
                Some(_) => continue, // ignore any other variant
                None => return None,
            }
        }
    })
    .await
    .expect("ShellRepaint within 2s")
    .expect("cmd_rx delivered a ShellRepaint");
    assert_eq!(
        cmd,
        (200, 60),
        "ShellRepaint must carry the cached term_size (200×60)"
    );

    client.close(None).await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server_task).await;
}

/// §D fallback: when no `TuiConfig` has arrived yet (the handle's
/// cached term_size is None), `ws_shell::handle` must still fire
/// `ShellRepaint` and use the (160, 50) defaults — matching rabbit's
/// `DEFAULT_TUI_COLS/DEFAULT_TUI_ROWS` and warren's
/// `TUI_WIDTH/TUI_HEIGHT` defaults. Pinned so the constants stay
/// in sync if anyone touches the rabbit-side defaults.
#[tokio::test]
async fn ws_shell_handle_repaint_uses_default_term_size_when_cached_is_none() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let registry = new_registry();
    let agent_id = uuid::Uuid::new_v4();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(8);
    let handle = AgentHandle::with_cmd_tx(agent_id, cmd_tx);
    // Leave term_size as None (the AgentStateSnapshot default).
    registry.entry(agent_id).or_insert_with(|| handle.clone());

    let server_registry = registry.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let transport = TungsteniteTransport::new(ws);
        let _ = ws_shell_handle(transport, server_registry, agent_id, false).await;
    });

    let connect = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut client = tokio_tungstenite::client_async("ws://localhost/socket", connect)
        .await
        .unwrap()
        .0;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let cmd = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match cmd_rx.recv().await {
                Some(Command::ShellRepaint { cols, rows }) => return Some((cols, rows)),
                Some(_) => continue,
                None => return None,
            }
        }
    })
    .await
    .expect("ShellRepaint within 2s")
    .expect("cmd_rx delivered a ShellRepaint");
    assert_eq!(
        cmd,
        (160, 50),
        "ShellRepaint must fall back to (160, 50) — the rabbit-side defaults"
    );

    client.close(None).await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server_task).await;
}

/// §Mobile-input: the shell WS must accept a typed `SendKey` envelope
/// from the client and translate it to a `Command::SendKeys` aimed at
/// the shell PTY (`TERM_CHAN_SHELL`). This is the contract that closes
/// the "iOS keyboard has no Tab/Escape/Arrow keys" gap on the
/// /agent/:id/shell page. The translation lives in `wire::key_to_bytes`
/// — this test only pins that the shell WS wires it through.
#[tokio::test]
async fn ws_shell_handle_dispatches_send_key_to_shell_pty() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let registry = new_registry();
    let agent_id = uuid::Uuid::new_v4();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(8);
    let handle = AgentHandle::with_cmd_tx(agent_id, cmd_tx);
    registry.entry(agent_id).or_insert_with(|| handle.clone());

    let server_registry = registry.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let transport = TungsteniteTransport::new(ws);
        let _ = ws_shell_handle(transport, server_registry, agent_id, false).await;
    });

    let connect = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut client, _upgrade) = tokio_tungstenite::client_async("ws://localhost/socket", connect)
        .await
        .unwrap();

    // Give the server a beat to drain the post-replay ShellRepaint so
    // cmd_rx only contains our SendKeys when we observe it.
    tokio::time::sleep(Duration::from_millis(150)).await;
    while cmd_rx.try_recv().is_ok() {}

    // Send a typed Tab key from the mobile chip palette.
    let env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 1,
        body: EnvelopeBody::SendKey(SendKey {
            key: Key::Tab,
            modifiers: None,
        }),
    };
    let frame = serde_json::to_string(&env).unwrap();
    client
        .send(tokio_tungstenite::tungstenite::Message::Text(frame))
        .await
        .unwrap();

    // Expect a Command::SendKeys aimed at the shell PTY with `\t`.
    let cmd = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match cmd_rx.recv().await {
                Some(Command::SendKeys { chan, data }) => return Some((chan, data)),
                Some(_) => continue,
                None => return None,
            }
        }
    })
    .await
    .expect("SendKeys within 2s")
    .expect("cmd_rx delivered a SendKeys");
    assert_eq!(cmd.0, TERM_CHAN_SHELL, "must target shell PTY");
    assert_eq!(cmd.1.as_ref(), b"\t", "Tab must translate to 0x09");

    // Viewer-mode connections must NOT fire keys — a watcher must not
    // be able to type into a peer's shell.
    // (Different agent, fresh WS, viewer_mode = true.)
    let viewer_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let viewer_addr = viewer_listener.local_addr().unwrap();
    let viewer_agent = uuid::Uuid::new_v4();
    let (viewer_cmd_tx, mut viewer_cmd_rx) = tokio::sync::mpsc::channel::<Command>(8);
    let viewer_handle = AgentHandle::with_cmd_tx(viewer_agent, viewer_cmd_tx);
    registry
        .entry(viewer_agent)
        .or_insert_with(|| viewer_handle.clone());
    let server_registry2 = registry.clone();
    let viewer_server = tokio::spawn(async move {
        let (stream, _peer) = viewer_listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let transport = TungsteniteTransport::new(ws);
        let _ = ws_shell_handle(transport, server_registry2, viewer_agent, true).await;
    });
    let viewer_connect = tokio::net::TcpStream::connect(viewer_addr).await.unwrap();
    let (mut viewer_client, _) =
        tokio_tungstenite::client_async("ws://localhost/socket", viewer_connect)
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    while viewer_cmd_rx.try_recv().is_ok() {}
    let env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 1,
        body: EnvelopeBody::SendKey(SendKey {
            key: Key::Escape,
            modifiers: None,
        }),
    };
    let frame = serde_json::to_string(&env).unwrap();
    viewer_client
        .send(tokio_tungstenite::tungstenite::Message::Text(frame))
        .await
        .unwrap();
    // Wait long enough that a stray SendKeys would arrive if the
    // server had wrongly accepted it; assert none did.
    let stray = tokio::time::timeout(Duration::from_millis(500), viewer_cmd_rx.recv()).await;
    assert!(
        stray.is_err(),
        "viewer-mode must drop SendKey envelopes, got {stray:?}"
    );

    viewer_client.close(None).await.ok();
    client.close(None).await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), server_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), viewer_server).await;
}

struct TungsteniteTransport<S> {
    inner: tokio_tungstenite::WebSocketStream<S>,
    close_reason: std::sync::Mutex<Option<rabbit_lib::server::CloseReason>>,
}

impl<S> TungsteniteTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn new(stream: tokio_tungstenite::WebSocketStream<S>) -> Self {
        Self {
            inner: stream,
            close_reason: std::sync::Mutex::new(None),
        }
    }
}

impl<S> futures_util::Stream for TungsteniteTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Item = Result<rabbit_lib::server::TransportMsg, std::io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use futures_util::Stream;
        let pinned = std::pin::Pin::new(&mut self.inner);
        match Stream::poll_next(pinned, cx) {
            std::task::Poll::Ready(Some(Ok(msg))) => {
                let mapped = match msg {
                    tokio_tungstenite::tungstenite::Message::Text(t) => {
                        rabbit_lib::server::TransportMsg::Text(t.to_string())
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        rabbit_lib::server::TransportMsg::Binary(b.to_vec())
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(p) => {
                        rabbit_lib::server::TransportMsg::Ping(p.to_vec())
                    }
                    tokio_tungstenite::tungstenite::Message::Pong(p) => {
                        rabbit_lib::server::TransportMsg::Pong(p.to_vec())
                    }
                    tokio_tungstenite::tungstenite::Message::Close(frame) => {
                        let reason = frame.map(|f| rabbit_lib::server::CloseReason {
                            code: f.code.into(),
                            reason: Some(f.reason.into_owned()),
                        });
                        *self.close_reason.lock().unwrap() = reason.clone();
                        rabbit_lib::server::TransportMsg::Close(reason)
                    }
                    tokio_tungstenite::tungstenite::Message::Frame(_) => {
                        rabbit_lib::server::TransportMsg::Binary(Vec::new())
                    }
                };
                std::task::Poll::Ready(Some(Ok(mapped)))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                std::task::Poll::Ready(Some(Err(std::io::Error::other(e.to_string()))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<S> futures_util::Sink<rabbit_lib::server::TransportMsg> for TungsteniteTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Error = std::io::Error;
    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        use futures_util::Sink;
        let pinned = std::pin::Pin::new(&mut self.inner);
        Sink::<tokio_tungstenite::tungstenite::Message>::poll_ready(pinned, cx)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: rabbit_lib::server::TransportMsg,
    ) -> Result<(), Self::Error> {
        use futures_util::Sink;
        let msg = match item {
            rabbit_lib::server::TransportMsg::Text(t) => {
                tokio_tungstenite::tungstenite::Message::Text(t)
            }
            rabbit_lib::server::TransportMsg::Binary(b) => {
                tokio_tungstenite::tungstenite::Message::Binary(b)
            }
            rabbit_lib::server::TransportMsg::Ping(p) => {
                tokio_tungstenite::tungstenite::Message::Ping(p.to_vec())
            }
            rabbit_lib::server::TransportMsg::Pong(p) => {
                tokio_tungstenite::tungstenite::Message::Pong(p.to_vec())
            }
            rabbit_lib::server::TransportMsg::Close(reason) => {
                let frame = reason.map(|r| rabbit_lib::server::CloseReason {
                    code: r.code,
                    reason: r.reason,
                });
                let frame = frame.map(|r| tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: r.code.into(),
                    reason: r.reason.unwrap_or_default().into(),
                });
                tokio_tungstenite::tungstenite::Message::Close(frame)
            }
        };
        let pinned = std::pin::Pin::new(&mut self.inner);
        Sink::<tokio_tungstenite::tungstenite::Message>::start_send(pinned, msg)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        use futures_util::Sink;
        let pinned = std::pin::Pin::new(&mut self.inner);
        Sink::<tokio_tungstenite::tungstenite::Message>::poll_flush(pinned, cx)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        use futures_util::Sink;
        let pinned = std::pin::Pin::new(&mut self.inner);
        Sink::<tokio_tungstenite::tungstenite::Message>::poll_close(pinned, cx)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

impl<S> WsTransport for TungsteniteTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn close_reason(&self) -> Option<rabbit_lib::server::CloseReason> {
        self.close_reason.lock().unwrap().clone()
    }
}
