//! Bridge between `axum::extract::ws::WebSocket` and the lib's
//! `WsTransport` trait.
//!
//! Relocated from `rabbit-lib::server::transport` once the lib dropped
//! its axum dependency. Embedders using axum call [`axum_ws_transport`]
//! to wrap an upgraded `WebSocket` as a `DynWsTransport`, then drive
//! it through the lib's framework-agnostic APIs (`actor::run`,
//! `ws_browser::handle`, `ws_shell::handle`, `ws_rabbit::handle_session`).

use std::io;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use futures_util::{Sink, Stream};
use rabbit_lib::server::{CloseReason, DynWsTransport, TransportMsg, WsTransport};

/// Wrap an `axum::extract::ws::WebSocket` as a [`DynWsTransport`].
pub fn axum_ws_transport(socket: axum::extract::ws::WebSocket) -> DynWsTransport {
    DynWsTransport::new(AxumWsBridge::new(socket))
}

struct AxumWsBridge {
    socket: axum::extract::ws::WebSocket,
    close_reason: Mutex<Option<CloseReason>>,
}

impl AxumWsBridge {
    fn new(socket: axum::extract::ws::WebSocket) -> Self {
        Self {
            socket,
            close_reason: Mutex::new(None),
        }
    }
}

impl Stream for AxumWsBridge {
    type Item = Result<TransportMsg, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SAFETY: `socket` is `Unpin` (it wraps an unpin `WebSocketStream`),
        // so projecting through `Pin::get_mut()` is safe.
        let this = self.get_mut();
        let sock = Pin::new(&mut this.socket);
        match sock.poll_next(cx) {
            Poll::Ready(Some(Ok(msg))) => {
                Poll::Ready(Some(Ok(axum_to_transport(msg, &this.close_reason))))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(io::Error::other(e.to_string())))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<TransportMsg> for AxumWsBridge {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        let sock = Pin::new(&mut this.socket);
        sock.poll_ready(cx)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn start_send(self: Pin<&mut Self>, item: TransportMsg) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let sock = Pin::new(&mut this.socket);
        sock.start_send(transport_to_axum(item))
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        let sock = Pin::new(&mut this.socket);
        sock.poll_flush(cx)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        let sock = Pin::new(&mut this.socket);
        sock.poll_close(cx)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

impl WsTransport for AxumWsBridge {
    fn close_reason(&self) -> Option<CloseReason> {
        self.close_reason.lock().unwrap().clone()
    }
}

fn axum_to_transport(
    msg: axum::extract::ws::Message,
    close_state: &Mutex<Option<CloseReason>>,
) -> TransportMsg {
    use axum::extract::ws::Message;
    match msg {
        Message::Text(t) => TransportMsg::Text(t),
        Message::Binary(b) => TransportMsg::Binary(b),
        Message::Ping(p) => TransportMsg::Ping(p),
        Message::Pong(p) => TransportMsg::Pong(p),
        Message::Close(frame) => {
            let reason = frame.map(|f| CloseReason {
                code: f.code,
                reason: Some(f.reason.into_owned()),
            });
            *close_state.lock().unwrap() = reason.clone();
            TransportMsg::Close(reason)
        }
    }
}

fn transport_to_axum(msg: TransportMsg) -> axum::extract::ws::Message {
    use axum::extract::ws::{CloseFrame, Message};
    match msg {
        TransportMsg::Text(t) => Message::Text(t),
        TransportMsg::Binary(b) => Message::Binary(b),
        TransportMsg::Ping(p) => Message::Ping(p),
        TransportMsg::Pong(p) => Message::Pong(p),
        TransportMsg::Close(reason) => {
            let frame = reason.map(|r| CloseFrame {
                code: r.code,
                reason: r.reason.unwrap_or_default().into(),
            });
            Message::Close(frame)
        }
    }
}
