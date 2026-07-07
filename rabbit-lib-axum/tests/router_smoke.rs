//! Phase 5 verification: build a `ServerState` with `AlwaysAllowAuth`,
//! spin up `rabbit_lib_axum::router` on a `tokio::net::TcpListener`,
//! hit `/api/agents/<uuid>/claude/state` with a stub request, assert
//! 200.

use std::sync::Arc;

use rabbit_lib::server::{
    registry::new_registry, AgentEventRecord, AuthBackend, AuthError, ServerState, SessionStore,
};
use rabbit_lib_axum::router;

#[derive(Default)]
struct AlwaysAllowAuth;

#[async_trait::async_trait]
impl AuthBackend for AlwaysAllowAuth {
    async fn authenticate_agent(
        &self,
        _headers: &http::HeaderMap,
    ) -> Result<uuid::Uuid, AuthError> {
        Ok(uuid::Uuid::nil())
    }
    async fn authenticate_admin(&self, _headers: &http::HeaderMap) -> Result<bool, AuthError> {
        Ok(true)
    }
}

struct EmptyStore;

#[async_trait::async_trait]
impl SessionStore for EmptyStore {
    async fn next_event_seq(
        &self,
        _agent_id: uuid::Uuid,
    ) -> Result<i64, rabbit_lib::server::StoreError> {
        Ok(1)
    }
    async fn insert_event(
        &self,
        _agent_id: uuid::Uuid,
        _seq: i64,
        _kind: &str,
        _payload_json: &str,
    ) -> Result<(), rabbit_lib::server::StoreError> {
        Ok(())
    }
    async fn insert_event_value(
        &self,
        _agent_id: uuid::Uuid,
        _seq: i64,
        _kind: &str,
        _payload: serde_json::Value,
    ) -> Result<(), rabbit_lib::server::StoreError> {
        Ok(())
    }
    async fn list_events_since(
        &self,
        _agent_id: uuid::Uuid,
        _since: i64,
        _limit: u64,
    ) -> Result<Vec<AgentEventRecord>, rabbit_lib::server::StoreError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn state_endpoint_returns_200_for_unknown_agent() {
    let store: Arc<dyn SessionStore> = Arc::new(EmptyStore);
    let auth: Arc<dyn AuthBackend> = Arc::new(AlwaysAllowAuth);
    let log_sink: Arc<dyn rabbit_lib::server::LogSink> = Arc::new(rabbit_lib::server::StdLogSink);
    let state = Arc::new(ServerState {
        registry: new_registry(),
        store,
        auth,
        log_sink,
    });

    // Listen on a random port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // The router takes `Arc<ServerState>` as its declared state. We
    // finalize with `.with_state(state.clone())` so the merged router
    // type-erases to `Router<()>` and `tower::ServiceExt::oneshot`
    // can drive it without a real socket.
    let app = router(state.clone()).with_state(state.clone());

    let agent_id = uuid::Uuid::new_v4();
    let req = http::Request::builder()
        .method("GET")
        .uri(format!("http://{addr}/api/agents/{agent_id}/claude/state"))
        .body(axum::body::Body::empty())
        .unwrap();

    // Drop the listener — we never actually accept; `oneshot` calls
    // into the router's service directly.
    drop(listener);

    let resp = tower::ServiceExt::oneshot(app, req)
        .await
        .expect("response");
    assert_eq!(resp.status(), http::StatusCode::OK);
}
