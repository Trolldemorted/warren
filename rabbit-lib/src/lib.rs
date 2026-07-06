//! `rabbit-lib` — the server-side runtime that warren embeds to fan
//! rabbit-supervisor WebSocket frames out to many browser subscribers
//! per agent, plus the wire-protocol types both halves serialize over.
//!
//! The supervisor half of the protocol (PTY wrapping, lifecycle hooks,
//! transcript tailing, the link layer, the meta replay ring, etc.) lives
//! in the `rabbit` crate as a private implementation detail of the
//! `rabbit` and `rabbit-hook` binaries. Only the shared `wire` types
//! and the server-side `server` runtime are published from this crate.
//!
//! Embedders construct a `ServerState` with concrete `SessionStore` /
//! `AuthBackend` implementations and merge `ServerState::router()` into
//! their own axum router.

pub mod server;
pub mod wire;
