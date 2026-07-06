# rabbit-lib

Remote-controllable [Claude](https://claude.ai) supervisor + matching
server-side runtime. The library that powers the
[Warren](https://github.com/warren/warren) agent supervisor: spawn
`claude` in a PTY, parse its terminal output, observe its lifecycle
via Claude's hook protocol, optionally record asciicast, and bridge
everything to a single WebSocket. Pair with the matching server
half to fan term-bytes and meta-envelopes out to many browser
subscribers per agent.

> **Status:** both halves of the rabbit protocol now live here — the
> supervisor (`pty`, `observer`, `link`, `supervisor`, `recorder`,
> `shell`, `trust`, `vt`, etc.) and the server half (`handle`,
> `actor`, `registry`, `ws_rabbit`, `ws_browser`, `ws_shell`, `http`).
> Embedders consume the lib through a `ServerState` constructed with
> concrete `SessionStore` / `AuthBackend` implementations.
> `rabbit-lib.md` documents the migration and the wire stability
> contract.

## Quick start

```rust,no_run
use rabbit_lib::{config::Config, supervisor};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    supervisor::run(cfg).await
}
```

Or, if you only want the protocol types without the PTY plumbing:

```rust
use rabbit_lib::wire::{Envelope, EnvelopeBody, PROTOCOL_VERSION};

fn parse(text: &str) -> serde_json::Result<Envelope> {
    serde_json::from_str(text)
}
```

## Feature flags

| Flag         | Default | What it gates                                                  |
|--------------|---------|----------------------------------------------------------------|
| `tls`        | **on**  | rustls TLS connector on `tokio-tungstenite` (for `wss://`)     |
| `shell`      | off     | the bash PTY sidecar (`rabbit_lib::shell`)                     |
| `asciicast`  | off     | the asciicast v2 recorder + http server                        |

`tls` is on by default because production deployments normally
reach the broker over TLS. Strip it with
`default-features = false` if you terminate TLS upstream.

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
runtime, so the same supervisor code can be embedded in any host:

```rust,no_run
use std::sync::Arc;
use rabbit_lib::server::{
    AgentEventRecord, AuthBackend, AuthError, LogSink, ServerState, SessionStore,
    StdLogSink, new_registry,
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
