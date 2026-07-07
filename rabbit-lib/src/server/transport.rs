//! Framework-agnostic WebSocket transport abstraction.
//!
//! The actor and the browser/shell WS handlers used to take
//! `axum::extract::ws::WebSocket` directly and pattern-match
//! `axum::extract::ws::Message` variants. That hard-couples
//! `rabbit-lib`'s public surface to axum. This module defines a
//! transport trait that any `AsyncRead + AsyncWrite` (or any other
//! WS-shaped stream) can implement, so embedders using hyper, actix,
//! a tokio-tungstenite direct path, or FFI bindings can drive the
//! same runtime without going through axum.
//!
//! The actor's `run` takes `impl WsTransport` and calls
//! `stream.split()` on it (futures-util's split, which works for any
//! `Stream + Sink`) to obtain the read-half `SplitStream` and
//! write-half `SplitSink` for the `tokio::select!` loop. Each
//! variant of `TransportMsg` is what the actor and the browser/shell
//! handlers consume and produce.

use futures_util::{Sink, Stream};
use std::io;

/// One frame of WebSocket I/O, framework-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportMsg {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<CloseReason>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReason {
    pub code: u16,
    pub reason: Option<String>,
}

/// Framework-agnostic WebSocket transport. Implementors wrap the
/// concrete WS codec from any web framework (axum, hyper, actix,
/// raw tokio-tungstenite, …) or any FFI-side byte stream with a
/// tungstenite handshake.
///
/// The trait requires `Stream + Sink` over [`TransportMsg`] so the
/// actor can call `stream.split()` (futures-util) on it and obtain
/// independent read/write halves for `tokio::select!`. Embedders
/// whose framework hands back split halves (axum's
/// `WebSocket::split()` returns `(ReadHalf, WriteHalf)`) wrap each
/// half in its own `WsTransport` impl and pass the read half's
/// `next()` + the write half's `send()` through `DynWsTransport` —
/// see the canonical `rabbit_lib_axum::ws_transport` adapter.
pub trait WsTransport:
    Stream<Item = Result<TransportMsg, io::Error>>
    + Sink<TransportMsg, Error = io::Error>
    + Unpin
    + Send
{
    /// Best-effort close reason captured at handshake time. `None`
    /// when the peer is still open or when the transport doesn't
    /// surface this information.
    fn close_reason(&self) -> Option<CloseReason>;
}

/// Type-erased `WsTransport`. Useful when an embedder wants to
/// store a transport behind `dyn` or hand it back from a function
/// that can't name the concrete framework half type.
pub struct DynWsTransport(pub Box<dyn WsTransport>);

impl DynWsTransport {
    pub fn new<T: WsTransport + 'static>(t: T) -> Self {
        Self(Box::new(t))
    }
}

impl std::fmt::Debug for DynWsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DynWsTransport").finish()
    }
}

impl Stream for DynWsTransport {
    type Item = Result<TransportMsg, io::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // `WsTransport: Unpin` (supertrait bound) ⇒ `dyn WsTransport: Unpin`,
        // so dereferencing the Box to its dyn target and pinning it is safe.
        std::pin::Pin::new(&mut *self.0).poll_next(cx)
    }
}

impl Sink<TransportMsg> for DynWsTransport {
    type Error = io::Error;
    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut *self.0).poll_ready(cx)
    }
    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: TransportMsg,
    ) -> Result<(), Self::Error> {
        std::pin::Pin::new(&mut *self.0).start_send(item)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut *self.0).poll_flush(cx)
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut *self.0).poll_close(cx)
    }
}

impl WsTransport for DynWsTransport {
    fn close_reason(&self) -> Option<CloseReason> {
        self.0.close_reason()
    }
}

#[cfg(test)]
mod tests {
    //! The transport abstraction is small enough that most of its
    //! surface gets exercised by the actor's downstream tests
    //! (`actor::tests::mock_transport_*`). Here we pin the trait's
    //! elementary guarantees: a `DynWsTransport` round-trips every
    //! `TransportMsg` variant and preserves the close reason.

    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CapturedState {
        close_reason: Option<CloseReason>,
    }

    struct MockTransport {
        inbound: tokio::sync::mpsc::UnboundedReceiver<TransportMsg>,
        outbound: Arc<Mutex<Vec<TransportMsg>>>,
        state: Arc<Mutex<CapturedState>>,
    }

    impl MockTransport {
        fn new() -> (
            Self,
            tokio::sync::mpsc::UnboundedSender<TransportMsg>,
            Arc<Mutex<Vec<TransportMsg>>>,
        ) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let out = Arc::new(Mutex::new(Vec::new()));
            let state = Arc::new(Mutex::new(CapturedState::default()));
            (
                Self {
                    inbound: rx,
                    outbound: out.clone(),
                    state: state.clone(),
                },
                tx,
                out,
            )
        }
    }

    impl Stream for MockTransport {
        type Item = Result<TransportMsg, io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let opt = std::pin::Pin::new(&mut self.inbound).poll_recv(cx);
            if let std::task::Poll::Ready(Some(TransportMsg::Close(reason))) = &opt {
                self.state.lock().unwrap().close_reason = reason.clone();
            }
            opt.map(|opt| opt.map(Ok))
        }
    }

    impl Sink<TransportMsg> for MockTransport {
        type Error = io::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(
            self: std::pin::Pin<&mut Self>,
            item: TransportMsg,
        ) -> Result<(), Self::Error> {
            if let TransportMsg::Close(reason) = &item {
                let mut state = self.state.lock().unwrap();
                state.close_reason = reason.clone();
            }
            self.outbound.lock().unwrap().push(item);
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl WsTransport for MockTransport {
        fn close_reason(&self) -> Option<CloseReason> {
            self.state.lock().unwrap().close_reason.clone()
        }
    }

    #[tokio::test]
    async fn dyn_ws_transport_round_trips_every_variant() {
        let (transport, sender, outbound) = MockTransport::new();
        let mut dyn_t = DynWsTransport::new(transport);

        // Send one of each non-Close variant on the write side. Each
        // arrives at `outbound` in order; `MockTransport::start_send`
        // captures every frame verbatim.
        for msg in [
            TransportMsg::Text("hello".into()),
            TransportMsg::Binary(vec![0x01, 0x02, 0x03]),
            TransportMsg::Ping(vec![0xAA]),
            TransportMsg::Pong(vec![0xBB]),
        ] {
            dyn_t.send(msg).await.expect("send");
        }

        // Inbound Close updates close_reason as a side effect of
        // `poll_next` returning the frame — we consume one item to
        // trigger that update.
        sender
            .send(TransportMsg::Close(Some(CloseReason {
                code: 1000,
                reason: Some("normal".into()),
            })))
            .expect("send close");

        let inbound_close = dyn_t
            .next()
            .await
            .expect("inbound item")
            .expect("inbound ok");
        match inbound_close {
            TransportMsg::Close(_) => {}
            other => panic!("expected Close, got {other:?}"),
        }

        let outbound = outbound.lock().unwrap();
        assert_eq!(outbound.len(), 4);
        assert_eq!(outbound[0], TransportMsg::Text("hello".into()));
        assert_eq!(outbound[1], TransportMsg::Binary(vec![0x01, 0x02, 0x03]));
        assert_eq!(outbound[2], TransportMsg::Ping(vec![0xAA]));
        assert_eq!(outbound[3], TransportMsg::Pong(vec![0xBB]));
        drop(outbound);

        let reason = dyn_t.close_reason().expect("close_reason captured");
        assert_eq!(reason.code, 1000);
        assert_eq!(reason.reason.as_deref(), Some("normal"));
    }

    #[tokio::test]
    async fn split_works_on_dyn_ws_transport() {
        let (transport, sender, outbound) = MockTransport::new();
        let dyn_t = DynWsTransport::new(transport);

        // futures-util's `StreamExt::split` returns `(SplitSink, SplitStream)`
        // — the sink half first. Use UFCS to pick the stream-side split
        // explicitly; both `StreamExt` and `SinkExt` define `split` and the
        // orderings differ.
        let (mut write, mut read) = futures_util::StreamExt::split(dyn_t);

        write
            .send(TransportMsg::Text("from-write-half".into()))
            .await
            .expect("write");
        sender
            .send(TransportMsg::Text("from-inbound".into()))
            .expect("send inbound");

        let read_msg = read.next().await.expect("read item").expect("read ok");
        assert_eq!(read_msg, TransportMsg::Text("from-inbound".into()));

        let outbound = outbound.lock().unwrap();
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound[0], TransportMsg::Text("from-write-half".into()));
    }
}
