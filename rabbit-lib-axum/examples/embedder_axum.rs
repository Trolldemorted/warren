//! Smoke test for `rabbit-lib-axum` over a real TCP socket.
//!
//! Builds a `ServerState` with `AlwaysAllowAuth` + `MockSessionStore`,
//! spins up `rabbit_lib_axum::router` on `127.0.0.1:0`, drives an
//! actual `tokio::net::TcpStream` HTTP request through it, and asserts
//! 200 OK on the `/api/agents/<uuid>/claude/state` endpoint. This
//! exercises the full axum stack the way warren does, not just the
//! in-process `oneshot` smoke test in `tests/router_smoke.rs`.
//!
//! Run with:
//!   cargo run -p rabbit-lib-axum --example embedder_axum
//!
//! Exits 0 on success, prints non-zero on assertion failure.

use std::sync::Arc;

use rabbit_lib::server::mock::{AlwaysAllowAuth, MockSessionStore};
use rabbit_lib::server::registry::new_registry;
use rabbit_lib::server::{LogSink, ServerState, StdLogSink};
use rabbit_lib_axum::router;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let store: Arc<dyn rabbit_lib::server::SessionStore> = Arc::new(MockSessionStore::default());
    let auth: Arc<dyn rabbit_lib::server::AuthBackend> = Arc::new(AlwaysAllowAuth::default());
    let log_sink: Arc<dyn LogSink> = Arc::new(StdLogSink);
    let state = Arc::new(ServerState {
        registry: new_registry(),
        store,
        auth,
        log_sink,
    });

    // The router takes `Arc<ServerState>` as its declared state. We
    // finalize with `.with_state(state.clone())` so the merged router
    // type-erases to `Router<()>` for `axum::serve`.
    let app = router(state.clone()).with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    eprintln!("embedder_axum listening on http://{addr}");

    let server_task = tokio::spawn(async move { axum::serve(listener, app).await });

    // Drive an actual HTTP request through the stack.
    let agent_id = uuid::Uuid::new_v4();
    let req = format!(
        "GET /api/agents/{agent_id}/claude/state HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    );

    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    tokio::io::AsyncWriteExt::write_all(&mut stream, req.as_bytes()).await?;
    stream.shutdown().await.ok();

    let mut buf = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut buf).await?;
    let response = String::from_utf8_lossy(&buf);
    let status_line = response.lines().next().unwrap_or("");
    eprintln!("status line: {status_line}");

    if !status_line.contains(" 200 ") {
        eprintln!("expected 200, got: {status_line}");
        eprintln!("full response: {response}");
        std::process::exit(2);
    }
    eprintln!("OK: got 200 from /api/agents/<uuid>/claude/state");

    server_task.abort();
    Ok(())
}
