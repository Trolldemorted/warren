# rabbit-lib

Server-side runtime for the rabbit agent-broker protocol. Embedders
mount `ServerState::router()` into their own axum router to fan
rabbit-supervisor WebSocket frames out to many browser subscribers
per agent and persist the event stream via a `SessionStore` trait
implementation.

The supervisor half (PTY wrapping, lifecycle hooks, transcript
tailing, the link layer) lives in the `rabbit` crate. This crate
ships only the parts the server needs: the shared wire types
(`wire`) and the broker runtime (`server`).

## Quick start

```rust
use rabbit_lib::wire::{Envelope, EnvelopeBody, PROTOCOL_VERSION};

fn parse(text: &str) -> serde_json::Result<Envelope> {
    serde_json::from_str(text)
}
```

## Wire stability contract

The `wire` module and the `PROTOCOL_VERSION` constant are a public
API. We commit to:

1. **`PROTOCOL_VERSION = 2` is the floor.** Bumping is a major-version
   semver event on `rabbit-lib`.
2. **`Envelope { v, seq, body }` JSON shape is unchanged.** Tags are
   snake_case (`#[serde(tag = "t", rename_all = "snake_case")]`).
   `ScreenSnapshotBody::after_seq` retains `#[serde(default)]` for
   v1-readability.
3. **Binary frame layout stays `<chan:1> <seq:8 BE u64> <bytes…>`** —
   the seq-numbered snapshot protocol described in
   `seq-numbered-snapshot-protocol.md`.
4. **HTTP routes mounted by `ServerState::router()` keep the same
   paths and shapes.**

The first three are the surface that supervisors and broker servers
exchange. The fourth is the surface a Warren-style embedder mounts.

## Embedding the server half

The server half is intentionally a trait surface, not a concrete
runtime, so the same broker code can be embedded in any host:

```rust,no_run
use std::sync::Arc;
use rabbit_lib::server::{
    new_registry, AgentEventRecord, AuthBackend, AuthError, LogSink,
    ServerState, SessionStore, StdLogSink,
};
use axum::http::HeaderMap;
use uuid::Uuid;

// 1. Implement SessionStore for your storage backend.
struct MyStore;
#[async_trait::async_trait]
impl SessionStore for MyStore {
    async fn next_event_seq(&self, _: Uuid) -> anyhow::Result<i64> { unimplemented!() }
    async fn insert_event(&self, _: Uuid, _: i64, _: &str,
                          _: serde_json::Value) -> anyhow::Result<()> { unimplemented!() }
    async fn list_events_since(&self, _: Uuid, _: i64, _: u64)
        -> anyhow::Result<Vec<AgentEventRecord>> { unimplemented!() }
}

// 2. Implement AuthBackend for your authn path.
struct MyAuth;
#[async_trait::async_trait]
impl AuthBackend for MyAuth {
    async fn authenticate_agent(&self, _: &HeaderMap) -> Result<Uuid, AuthError> { unimplemented!() }
    async fn authenticate_admin(&self, _: &HeaderMap) -> Result<bool, AuthError> { unimplemented!() }
}

// 3. Build a ServerState and mount its router.
let state = Arc::new(ServerState {
    registry: new_registry(),
    store: Arc::new(MyStore),
    auth: Arc::new(MyAuth),
    log_sink: Arc::new(StdLogSink),
});
let app = state.router();
// axum::serve(axum::serve::DefaultMakeService, app)...
```

## MSRV

`rust-version = "1.85"` (workspace-wide).

## License

MIT OR Apache-2.0.