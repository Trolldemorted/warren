//! Non-axum embedder proof.
//!
//! `#[tokio::main]` program that:
//! 1. Builds a `ServerState` with `AlwaysAllowAuth` + an in-memory
//!    `SessionStore` shim.
//! 2. Listens on `tokio::net::TcpListener`.
//! 3. Accepts one connection via `tokio_tungstenite::accept_async`.
//! 4. Wraps the resulting `WebSocketStream` as a `DynWsTransport`
//!    using a tiny adapter (`TungsteniteWs` below — embedders using
//!    tokio-tungstenite directly can copy this verbatim).
//! 5. Drives `handle_session`, which runs the actor loop and
//!    publishes / consumes messages via the WS codec.
//!
//! Run with:
//!   cargo run -p rabbit-lib --example embedder_tokio
//!
//! Then in another terminal:
//!   websocat ws://127.0.0.1:7878
//! or:
//!   python3 -c "import asyncio, websockets; ..." (etc.)

use std::sync::Arc;

use rabbit_lib::server::ws_rabbit::handle_session;
use rabbit_lib::server::{
    mock::AlwaysAllowAuth, registry::new_registry, AgentEventRecord, CloseReason, DynWsTransport,
    ServerState, SessionStore, StoreError, TransportMsg, WsTransport,
};
use rabbit_lib::wire::{AgentState, EnvelopeBody};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Debug)
        .init()
        .ok();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:7878").await?;
    eprintln!("embedder_tokio listening on ws://127.0.0.1:7878");

    // Construct a `ServerState` with mock backends. A real embedder
    // would plug in their `AuthBackend` / `SessionStore` impls.
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::default());
    let auth: Arc<dyn rabbit_lib::server::AuthBackend> = Arc::new(AlwaysAllowAuth::default());
    let log_sink: Arc<dyn rabbit_lib::server::LogSink> = Arc::new(rabbit_lib::server::StdLogSink);
    let state = Arc::new(ServerState {
        registry: new_registry(),
        store,
        auth,
        log_sink,
    });

    loop {
        let (stream, _peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let ws = match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("ws handshake failed: {e:?}");
                    return;
                }
            };
            let transport: DynWsTransport = DynWsTransport::new(TungsteniteWs::new(ws));
            // In production, the agent_id would come from the auth
            // backend's classification of the bearer token / cookie.
            let agent_id = uuid::Uuid::new_v4();
            let store = state.store.clone();
            let registry = state.registry.clone();
            if let Err(e) = handle_session(store, registry, transport, agent_id).await {
                eprintln!("handle_session ended: {e:?}");
            }
        });
    }
}

// ----- In-memory session store (for the example) -----

#[derive(Default)]
struct InMemoryStore {
    rows: parking_lot::Mutex<Vec<(uuid::Uuid, i64, String)>>,
    seq: parking_lot::Mutex<i64>,
}

#[async_trait::async_trait]
impl SessionStore for InMemoryStore {
    async fn next_event_seq(&self, _agent_id: uuid::Uuid) -> Result<i64, StoreError> {
        let mut s = self.seq.lock();
        let v = *s;
        *s += 1;
        Ok(v)
    }
    async fn insert_event(
        &self,
        agent_id: uuid::Uuid,
        seq: i64,
        kind: &str,
        _payload_json: &str,
    ) -> Result<(), StoreError> {
        self.rows.lock().push((agent_id, seq, kind.to_string()));
        Ok(())
    }
    async fn insert_event_value(
        &self,
        agent_id: uuid::Uuid,
        seq: i64,
        kind: &str,
        _payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        self.rows.lock().push((agent_id, seq, kind.to_string()));
        Ok(())
    }
    async fn list_events_since(
        &self,
        _agent_id: uuid::Uuid,
        _since: i64,
        _limit: u64,
    ) -> Result<Vec<AgentEventRecord>, StoreError> {
        Ok(Vec::new())
    }
}

// ----- tokio-tungstenite → WsTransport adapter -----

struct TungsteniteWs<S> {
    inner: tokio_tungstenite::WebSocketStream<S>,
    close_reason: parking_lot::Mutex<Option<CloseReason>>,
}

impl<S> TungsteniteWs<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn new(stream: tokio_tungstenite::WebSocketStream<S>) -> Self {
        Self {
            inner: stream,
            close_reason: parking_lot::Mutex::new(None),
        }
    }
}

impl<S> futures_util::Stream for TungsteniteWs<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    type Item = Result<TransportMsg, std::io::Error>;

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
                        TransportMsg::Text(t.to_string())
                    }
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        TransportMsg::Binary(b.to_vec())
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(p) => {
                        TransportMsg::Ping(p.to_vec())
                    }
                    tokio_tungstenite::tungstenite::Message::Pong(p) => {
                        TransportMsg::Pong(p.to_vec())
                    }
                    tokio_tungstenite::tungstenite::Message::Close(frame) => {
                        let reason = frame.map(|f| CloseReason {
                            code: f.code.into(),
                            reason: Some(f.reason.into_owned()),
                        });
                        *self.close_reason.lock() = reason.clone();
                        TransportMsg::Close(reason)
                    }
                    tokio_tungstenite::tungstenite::Message::Frame(_) => {
                        TransportMsg::Binary(Vec::new())
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

impl<S> futures_util::Sink<TransportMsg> for TungsteniteWs<S>
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
        item: TransportMsg,
    ) -> Result<(), Self::Error> {
        use futures_util::Sink;
        let msg = match item {
            TransportMsg::Text(t) => tokio_tungstenite::tungstenite::Message::Text(t),
            TransportMsg::Binary(b) => tokio_tungstenite::tungstenite::Message::Binary(b),
            TransportMsg::Ping(p) => tokio_tungstenite::tungstenite::Message::Ping(p),
            TransportMsg::Pong(p) => tokio_tungstenite::tungstenite::Message::Pong(p),
            TransportMsg::Close(reason) => {
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

impl<S> WsTransport for TungsteniteWs<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn close_reason(&self) -> Option<CloseReason> {
        self.close_reason.lock().clone()
    }
}

// Reference `EnvelopeBody` / `AgentState` so the example still compiles
// when wire.rs evolves (the example doesn't need them, but they're the
// canonical wire types an embedder might inspect).
const _: fn() = || {
    let _ = std::mem::size_of::<EnvelopeBody>();
    let _ = std::mem::size_of::<AgentState>();
};
