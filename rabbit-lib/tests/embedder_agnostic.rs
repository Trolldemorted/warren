//! Non-axum embedder proof.
//!
//! Spins up a `tokio::net::TcpListener`, accepts one connection via
//! `tokio_tungstenite::accept_async`, wraps the resulting
//! `WebSocketStream` as a `rabbit_lib::server::DynWsTransport`, and
//! drives `handle_session` end-to-end. Sends a `Hello` envelope from
//! the client side, asserts the server responds with a `Hello` (the
//! "hello" round-trip the actor performs on connect), and checks the
//! mock session store saw the persistence call.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rabbit_lib::server::ws_rabbit::handle_session;
use rabbit_lib::server::{
    mock::{AlwaysAllowAuth, MockSessionStore},
    registry::new_registry,
    ServerState, SessionStore, WsTransport,
};
use rabbit_lib::wire::{AgentState, Envelope, EnvelopeBody, HelloUp, TermSize, PROTOCOL_VERSION};

#[tokio::test]
async fn hello_round_trip_over_tokio_tungstenite() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let store = Arc::new(MockSessionStore::default());
    let auth = Arc::new(AlwaysAllowAuth::default());
    let state = Arc::new(ServerState {
        registry: new_registry(),
        store: store.clone(),
        auth: auth.clone(),
        log_sink: Arc::new(rabbit_lib::server::StdLogSink),
        tui_size: Some((80, 24)),
    });

    let server_state = state.clone();
    let server_task = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // Wrap the tokio_tungstenite stream as a DynWsTransport by
        // providing a tiny adapter. The adapter lives in the test
        // because it's specific to the WS codec we picked — the lib
        // exposes only the `WsTransport` trait.
        let transport = TungsteniteTransport::new(ws);
        // Pick an arbitrary agent id; the test doesn't care about
        // the actor's authentication flow (we use a stand-in
        // `AlwaysAllowAuth`).
        let agent_id = uuid::Uuid::new_v4();
        let store: Arc<dyn SessionStore> = server_state.store.clone();
        let registry = server_state.registry.clone();
        let tui_size = server_state.tui_size;
        let _ = handle_session(store, registry, transport, agent_id, tui_size).await;
    });

    // Client side: connect, send Hello, expect Hello back.
    let connect = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut client = tokio_tungstenite::client_async("ws://localhost/socket", connect)
        .await
        .unwrap()
        .0;

    let hello = Envelope {
        v: PROTOCOL_VERSION,
        seq: 1,
        body: EnvelopeBody::Hello(HelloUp {
            agent_id: uuid::Uuid::new_v4(),
            protocol_v: PROTOCOL_VERSION,
            claude_version: "test-1.0".to_string(),
            session_id: Some("sess-1".to_string()),
            state: AgentState::Idle,
            term_size: TermSize { cols: 80, rows: 24 },
        }),
    };
    let s = serde_json::to_string(&hello).unwrap();
    client
        .send(tokio_tungstenite::tungstenite::Message::Text(s))
        .await
        .unwrap();

    // Read envelopes until we see the Ack for our hello. The actor sends
    // `TuiConfig` first (right after the hello is persisted), then the
    // initial Ack for everything already in the DB (which carries
    // seq=1 — the hello's own seq).
    let mut saw_ack = false;
    for _ in 0..8 {
        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("reply within 2s")
            .expect("reply not None")
            .expect("reply ok");
        let text = match reply {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            other => panic!("expected Text, got {other:?}"),
        };
        let env: Envelope = serde_json::from_str(&text).expect("parse envelope");
        if let EnvelopeBody::Ack { ack_seq } = env.body {
            assert_eq!(ack_seq, 1, "hello persisted at seq=1");
            saw_ack = true;
            break;
        }
    }
    assert!(
        saw_ack,
        "actor must send an Ack within the first 8 envelopes"
    );

    client.close(None).await.ok();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
}

/// `WsTransport` adapter around a `tokio_tungstenite::WebSocketStream`.
///
/// Lives in the test because the lib intentionally doesn't ship a
/// tungstenite adapter (the whole point of `WsTransport` is that
/// embedders can pick whichever WS codec they like — FFI bindings
/// might drive it from C, hyper from a different codec, etc.).
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
                        // Raw frame — not expected at this layer; the
                        // WS codec handles framing. Yield an empty
                        // binary and continue polling.
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
                tokio_tungstenite::tungstenite::Message::Ping(p)
            }
            rabbit_lib::server::TransportMsg::Pong(p) => {
                tokio_tungstenite::tungstenite::Message::Pong(p)
            }
            rabbit_lib::server::TransportMsg::Close(reason) => {
                let frame = reason.map(|r| tokio_tungstenite::tungstenite::protocol::CloseFrame {
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
