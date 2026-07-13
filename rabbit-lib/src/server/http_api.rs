//! Framework-agnostic domain functions backing the lib's HTTP API.
//!
//! Each `http_*` method on [`ServerState`] takes plain async arguments
//! (`&::http::HeaderMap`, agent id, typed request structs) and returns
//! a plain `Result<Domain, ServerError>`. The axum adapter layer in
//! `http.rs` is a thin translation between HTTP shape and these
//! domain calls.
//!
//! All endpoints check admin auth internally. Embedders who want to
//! bypass the auth gate (e.g. test harnesses) can call
//! `AgentHandle::prompt(...)` etc. directly on the `AgentRegistry`.
//!
//! Two SSE shapes are exposed for the event stream:
//! - [`ServerState::http_events_stream`] — envelope stream (raw
//!   `EnvelopeBody` items). Embedders write their own SSE framing.
//! - [`ServerState::http_events_stream_sse`] — pre-formatted SSE byte
//!   chunks (`data: {...}\n\n` plus keepalive comments). Embedders
//!   who just need to plug into an existing SSE pipeline.

use crate::server::handle::{AgentHandle, AgentStateSnapshot};
use crate::server::{AuthError, ServerError, ServerResult, ServerState};
use crate::wire::{AgentState, EnvelopeBody, UsageSnapshot};
use futures_util::stream::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

// ----- Public request / response structs -----

/// Body for `POST /api/agents/:id/claude/prompt`. Plain async domain
/// types — no axum / serde derive in this module's signatures.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub text: String,
    pub wait: bool,
}

#[derive(Debug, Clone)]
pub struct PromptResponse {
    pub prompt_id: Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
}

/// Body for `POST /api/agents/:id/claude/clear`.
#[derive(Debug, Clone, Default)]
pub struct ClearRequest {
    pub hard: bool,
}

/// Body for `POST /api/agents/:id/claude/restart`.
#[derive(Debug, Clone, Default)]
pub struct RestartRequest {
    pub fresh: bool,
}

/// Response for `GET /api/agents/:id/claude/state`.
#[derive(Debug, Clone)]
pub struct StateResponse {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub claude_version: Option<String>,
    pub connected: bool,
}

/// Query for `GET /api/agents/:id/claude/events` and the SSE endpoint.
#[derive(Debug, Clone, Default)]
pub struct EventsQuery {
    pub since: i64,
    pub limit: Option<u64>,
}

/// One event row for `GET /api/agents/:id/claude/events`.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub seq: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// The flattened usage snapshot returned by
/// `GET /api/agents/:id/claude/usage`.
#[derive(Debug, Clone, Default)]
pub struct UsageResponse {
    pub usage: UsageSnapshot,
}

// ----- Helpers -----

async fn require_handle(state: &Arc<ServerState>, agent_id: Uuid) -> ServerResult<AgentHandle> {
    state
        .registry
        .get(&agent_id)
        .map(|h| h.clone())
        .ok_or(ServerError::NotFound)
}

async fn check_admin(state: &Arc<ServerState>, headers: &::http::HeaderMap) -> ServerResult<()> {
    if state.auth.authenticate_admin(headers).await? {
        Ok(())
    } else {
        Err(ServerError::Auth(AuthError::Invalid))
    }
}

// ----- Domain functions on ServerState -----

impl ServerState {
    /// `POST /api/agents/:id/claude/prompt`. Admin-only.
    pub async fn http_prompt(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
        req: PromptRequest,
    ) -> ServerResult<PromptResponse> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        if req.text.trim().is_empty() {
            return Err(ServerError::BadRequest("text required".into()));
        }
        // Force `wait=true` so the actor's gate populates the oneshot with
        // a rejection error; `wait=false` would fabricate success and
        // hide the rejection.
        let outcome = handle.prompt(&req.text, /*wait=*/ true).await?;
        if let Some(err) = outcome.error.as_deref() {
            if err == "agent is running a turn"
                || err == "agent is dead"
                || err == "turn queue full"
            {
                return Err(ServerError::Conflict(err.into()));
            }
        }
        Ok(PromptResponse {
            prompt_id: outcome.prompt_id,
            started_at: outcome.started_at,
            ended_at: outcome.ended_at,
        })
    }

    /// `GET /api/agents/:id/claude/usage`. Admin-only.
    pub async fn http_usage(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<UsageResponse> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        let usage = handle.usage().await?;
        Ok(UsageResponse { usage })
    }

    /// `GET /api/agents/:id/claude/state`. Admin-only.
    pub async fn http_state(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<StateResponse> {
        check_admin(self, headers).await?;
        let connected = self.registry.contains_key(&id);
        let snap: AgentStateSnapshot = if connected {
            require_handle(self, id).await?.state().await?
        } else {
            AgentStateSnapshot::default()
        };
        Ok(StateResponse {
            state: snap.state,
            session_id: snap.session_id,
            claude_version: snap.claude_version,
            connected,
        })
    }

    /// `POST /api/agents/:id/claude/clear`. Admin-only.
    pub async fn http_clear(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
        req: ClearRequest,
    ) -> ServerResult<()> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        handle.clear(req.hard).await?;
        Ok(())
    }

    /// `POST /api/agents/:id/claude/compact`. Admin-only.
    pub async fn http_compact(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<()> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        handle.compact().await?;
        Ok(())
    }

    /// `POST /api/agents/:id/claude/interrupt`. Admin-only.
    pub async fn http_interrupt(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<()> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        handle.interrupt().await?;
        Ok(())
    }

    /// `POST /api/agents/:id/claude/usage_check`. Admin-only. Asks
    /// rabbit to drive the synchronous `/usage` overlay scrape;
    /// returns immediately (the parsed limits arrive on the SSE
    /// `/events/stream` channel a moment later inside a fresh `Usage`
    /// envelope). Forward-compatible with future warren bg-task
    /// schedulers that want to poll the same endpoint.
    pub async fn http_usage_check(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<()> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        handle.usage_check().await?;
        Ok(())
    }

    /// `POST /api/agents/:id/claude/context_check`. Admin-only. Asks
    /// rabbit to drive the synchronous `/context` overlay scrape;
    /// returns immediately (the parsed context-window usage arrives
    /// on the SSE `/events/stream` channel a moment later inside a
    /// fresh `Usage` envelope carrying the new `ctx_*` fields).
    /// Mirrors [`http_usage_check`](Self::http_usage_check) in shape
    /// and fire-and-forget semantics.
    pub async fn http_context_check(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<()> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        handle.context_check().await?;
        Ok(())
    }

    /// `POST /api/agents/:id/claude/restart`. Admin-only.
    pub async fn http_restart(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
        req: RestartRequest,
    ) -> ServerResult<()> {
        check_admin(self, headers).await?;
        let handle = require_handle(self, id).await?;
        handle.restart(req.fresh).await?;
        Ok(())
    }

    /// `GET /api/agents/:id/claude/events`. Admin-only. Returns a JSON
    /// list of event rows (since/limit query).
    pub async fn http_events(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
        q: EventsQuery,
    ) -> ServerResult<Vec<EventRow>> {
        check_admin(self, headers).await?;
        let limit = q.limit.unwrap_or(500).clamp(1, 5000);
        let rows = self.store.list_events_since(id, q.since, limit).await?;
        Ok(rows
            .into_iter()
            .map(|m| EventRow {
                id: m.id,
                agent_id: m.agent_id,
                seq: m.seq,
                ts: m.ts,
                kind: m.kind,
                payload: m.payload,
            })
            .collect())
    }

    /// `GET /api/agents/:id/claude/events/stream` — envelope shape.
    /// Returns a stream of `EnvelopeBody` items: callers do their own
    /// SSE framing. Admin-only. Async because the admin check is
    /// async; the returned stream is consumed independently of the
    /// request future.
    pub async fn http_events_stream(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<EnvelopeBodyStream> {
        check_admin(self, headers).await?;
        if !self.registry.contains_key(&id) {
            return Err(ServerError::NotFound);
        }
        let mut meta_rx = self
            .registry
            .get(&id)
            .ok_or(ServerError::NotFound)?
            .subscribe_meta();
        Ok(Box::pin(async_stream::stream! {
            loop {
                match meta_rx.recv().await {
                    Ok(body) => yield Ok::<EnvelopeBody, ServerError>(body),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }))
    }

    /// `GET /api/agents/:id/claude/events/stream` — pre-formatted SSE
    /// bytes (`data: {...}\n\n`, plus a `: keepalive\n\n` comment every
    /// 15 s). Admin-only. See [`Self::http_events_stream`] for why
    /// this is `async`.
    pub async fn http_events_stream_sse(
        self: &Arc<Self>,
        headers: &::http::HeaderMap,
        id: Uuid,
    ) -> ServerResult<SseBytesStream> {
        check_admin(self, headers).await?;
        if !self.registry.contains_key(&id) {
            return Err(ServerError::NotFound);
        }
        let mut meta_rx = self
            .registry
            .get(&id)
            .ok_or(ServerError::NotFound)?
            .subscribe_meta();
        Ok(Box::pin(async_stream::stream! {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume immediate tick
            loop {
                tokio::select! {
                    biased;
                    ev = meta_rx.recv() => match ev {
                        Ok(body) => {
                            let json = serde_json::to_string(&body).unwrap_or_default();
                            let chunk = format!("data: {json}\n\n");
                            yield Ok::<Vec<u8>, ServerError>(chunk.into_bytes());
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = ticker.tick() => {
                        yield Ok::<Vec<u8>, ServerError>(b": keepalive\n\n".to_vec());
                    }
                }
            }
        }))
    }
}

/// Stream type returned by [`ServerState::http_events_stream`].
pub type EnvelopeBodyStream = Pin<Box<dyn Stream<Item = ServerResult<EnvelopeBody>> + Send>>;

/// Stream type returned by [`ServerState::http_events_stream_sse`].
pub type SseBytesStream = Pin<Box<dyn Stream<Item = ServerResult<Vec<u8>>> + Send>>;
#[cfg(test)]
mod tests {
    //! Pin the http_* domain functions' happy paths and auth-fail paths.
    //! Each test wires a stub auth + a stub session store into a fresh
    //! `ServerState` and asserts on the typed return values (NOT on the
    //! HTTP wire shape — the adapter layer in `http.rs` is tested by
    //! the integration suite).
    use super::*;
    use crate::server::actor::{Command, TurnOutcomeMsg};
    use crate::server::handle::AgentStateSnapshot;
    use crate::server::SessionStore;
    use crate::wire::{AgentState, EnvelopeBody};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use uuid::Uuid;

    /// Auth that says "yes, you're an admin" if `accept_admin` is true.
    struct StubAuth {
        accept_admin: bool,
    }

    #[async_trait::async_trait]
    impl crate::server::AuthBackend for StubAuth {
        async fn authenticate_agent(
            &self,
            _headers: &::http::HeaderMap,
        ) -> Result<Uuid, crate::server::AuthError> {
            Err(crate::server::AuthError::Missing)
        }
        async fn authenticate_admin(
            &self,
            _headers: &::http::HeaderMap,
        ) -> Result<bool, crate::server::AuthError> {
            Ok(self.accept_admin)
        }
    }

    /// SessionStore stub that records the last `since` / `limit` it was
    /// asked for and returns the rows we pre-load.
    struct StubStore {
        rows: Vec<crate::server::AgentEventRecord>,
        last_since: std::sync::Mutex<Option<i64>>,
        last_limit: std::sync::Mutex<Option<u64>>,
    }

    #[async_trait::async_trait]
    impl SessionStore for StubStore {
        async fn next_event_seq(&self, _agent_id: Uuid) -> Result<i64, crate::server::StoreError> {
            Ok(1)
        }
        async fn insert_event(
            &self,
            _agent_id: Uuid,
            _seq: i64,
            _kind: &str,
            _payload_json: &str,
        ) -> Result<(), crate::server::StoreError> {
            Ok(())
        }
        async fn insert_event_value(
            &self,
            _agent_id: Uuid,
            _seq: i64,
            _kind: &str,
            _payload: serde_json::Value,
        ) -> Result<(), crate::server::StoreError> {
            Ok(())
        }
        async fn list_events_since(
            &self,
            _agent_id: Uuid,
            since: i64,
            limit: u64,
        ) -> Result<Vec<crate::server::AgentEventRecord>, crate::server::StoreError> {
            *self.last_since.lock().unwrap() = Some(since);
            *self.last_limit.lock().unwrap() = Some(limit);
            Ok(self.rows.clone())
        }
    }

    fn build_state(admin_ok: bool) -> (Arc<ServerState>, Arc<StubStore>) {
        let store = Arc::new(StubStore {
            rows: Vec::new(),
            last_since: std::sync::Mutex::new(None),
            last_limit: std::sync::Mutex::new(None),
        });
        let auth: Arc<dyn crate::server::AuthBackend> = Arc::new(StubAuth {
            accept_admin: admin_ok,
        });
        let log_sink: Arc<dyn crate::server::LogSink> = Arc::new(crate::server::StdLogSink);
        let state = Arc::new(ServerState {
            registry: crate::server::registry::new_registry(),
            store: store.clone(),
            auth,
            log_sink,
            tui_size: None,
        });
        (state, store)
    }

    fn admin_headers() -> ::http::HeaderMap {
        ::http::HeaderMap::new()
    }

    /// Register an agent in the registry and spawn a fake-actor task
    /// that drains commands + replies to oneshots. The fake actor
    /// mirrors the real actor's busy-gate so the busy-path tests
    /// drive end-to-end behavior.
    fn register_agent(state: &ServerState) -> Uuid {
        let id = Uuid::new_v4();
        let initial = state.registry.register(id);
        let (handle_for_actor, cmd_tx, mut cmd_rx) = initial.split_for_actor();
        if let Some(mut entry) = state.registry.get_mut(&id) {
            entry.value_mut().install_cmd_tx(cmd_tx);
        }
        // Keep the per-actor handle alive (it owns the broadcast senders
        // the registry subscribers use).
        let fake_handle = handle_for_actor.clone();
        // Spawn a fake actor that drains commands and replies to oneshots.
        tokio::spawn(async move {
            const FAKE_PENDING_CAP: usize = 16;
            let pending: VecDeque<(Uuid, oneshot::Sender<TurnOutcomeMsg>)> = VecDeque::new();
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::Prompt {
                        id,
                        wait,
                        reply,
                        by_connection_id,
                        ..
                    } => {
                        let snap = fake_handle.snapshot();
                        let reject_reason: Option<&'static str> = match snap.state {
                            AgentState::Running => Some("agent is running a turn"),
                            AgentState::Dead => Some("agent is dead"),
                            _ => None,
                        };
                        if let Some(reason) = reject_reason {
                            fake_handle.publish_meta(EnvelopeBody::PromptRejected {
                                id,
                                reason: reason.to_string(),
                                by_connection_id,
                            });
                            if wait {
                                if let Some(tx) = reply {
                                    let now = chrono::Utc::now();
                                    let _ = tx.send(TurnOutcomeMsg {
                                        prompt_id: id,
                                        started_at: now,
                                        ended_at: now,
                                        usage: None,
                                        error: Some(reason.to_string()),
                                    });
                                }
                            }
                            continue;
                        }
                        if wait && pending.len() >= FAKE_PENDING_CAP {
                            fake_handle.publish_meta(EnvelopeBody::PromptRejected {
                                id,
                                reason: "turn queue full".to_string(),
                                by_connection_id,
                            });
                            if let Some(tx) = reply {
                                let now = chrono::Utc::now();
                                let _ = tx.send(TurnOutcomeMsg {
                                    prompt_id: id,
                                    started_at: now,
                                    ended_at: now,
                                    usage: None,
                                    error: Some("turn queue full".to_string()),
                                });
                            }
                            continue;
                        }
                        if let (true, Some(tx)) = (wait, reply) {
                            let now = chrono::Utc::now();
                            let _ = tx.send(TurnOutcomeMsg {
                                prompt_id: id,
                                started_at: now,
                                ended_at: now,
                                usage: None,
                                error: None,
                            });
                        }
                    }
                    Command::Clear {
                        reply: Some(tx), ..
                    } => {
                        let _ = tx.send(());
                    }
                    _ => {}
                }
            }
        });
        id
    }

    #[tokio::test]
    async fn http_prompt_happy_path() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        let resp = state
            .http_prompt(
                &admin_headers(),
                id,
                PromptRequest {
                    text: "hello".into(),
                    wait: true,
                },
            )
            .await
            .expect("prompt ok");
        assert!(!resp.prompt_id.is_nil());
    }

    #[tokio::test]
    async fn http_prompt_auth_fail_returns_invalid() {
        let (state, _store) = build_state(false);
        let id = register_agent(&state);
        let err = state
            .http_prompt(
                &admin_headers(),
                id,
                PromptRequest {
                    text: "hello".into(),
                    wait: false,
                },
            )
            .await
            .expect_err("auth fail");
        assert!(matches!(
            err,
            ServerError::Auth(crate::server::AuthError::Invalid)
        ));
    }

    #[tokio::test]
    async fn http_usage_returns_handle_snapshot() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        let resp = state.http_usage(&admin_headers(), id).await.expect("usage");
        // Default-constructed UsageSnapshot serializes cleanly; we just
        // need the typed `UsageResponse` shape to round-trip.
        let _ = resp.usage;
    }

    #[tokio::test]
    async fn http_state_unknown_agent_returns_default_unconnected() {
        let (state, _store) = build_state(true);
        let bogus = Uuid::new_v4();
        let resp = state
            .http_state(&admin_headers(), bogus)
            .await
            .expect("state");
        assert!(!resp.connected);
        assert_eq!(resp.state, AgentState::Starting);
    }

    #[tokio::test]
    async fn http_clear_happy_path() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        state
            .http_clear(&admin_headers(), id, ClearRequest { hard: false })
            .await
            .expect("clear");
    }

    #[tokio::test]
    async fn http_compact_happy_path() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        state
            .http_compact(&admin_headers(), id)
            .await
            .expect("compact");
    }

    #[tokio::test]
    async fn http_interrupt_happy_path() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        state
            .http_interrupt(&admin_headers(), id)
            .await
            .expect("interrupt");
    }

    #[tokio::test]
    async fn http_restart_happy_path() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        state
            .http_restart(&admin_headers(), id, RestartRequest { fresh: true })
            .await
            .expect("restart");
    }

    #[tokio::test]
    async fn http_events_clamps_limit_to_5000() {
        let (state, store) = build_state(true);
        let id = register_agent(&state);
        let rows = state
            .http_events(
                &admin_headers(),
                id,
                EventsQuery {
                    since: 0,
                    limit: Some(99_999),
                },
            )
            .await
            .expect("events");
        assert!(rows.is_empty());
        let recorded_limit = *store.last_limit.lock().unwrap();
        // Server clamps 99_999 down to 5000 (the upper bound).
        assert_eq!(recorded_limit, Some(5000));
    }

    #[tokio::test]
    async fn http_prompt_busy_returns_conflict() {
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        // Force the handle into `Running`. AgentStateSnapshot fields are
        // private, but we can use `update_state` from a clone.
        let handle = state.registry.get(&id).expect("registered");
        let snapshot = AgentStateSnapshot {
            state: AgentState::Running,
            ..Default::default()
        };
        handle.update_state(snapshot);
        let err = state
            .http_prompt(
                &admin_headers(),
                id,
                PromptRequest {
                    text: "x".into(),
                    wait: false,
                },
            )
            .await
            .expect_err("busy");
        assert!(matches!(err, ServerError::Conflict(_)));
    }

    #[tokio::test]
    async fn http_events_stream_auth_fail_short_circuits() {
        // The SSE methods' domain shape: admin check first, then
        // stream construction. Verifying the auth-fail path is enough
        // to pin the contract; the actual broadcast loop is exercised
        // by the integration suite (which holds the registry's senders
        // alive across a real broadcast).
        let (state, _store) = build_state(false);
        let id = register_agent(&state);
        match state.http_events_stream(&admin_headers(), id).await {
            Err(ServerError::Auth(crate::server::AuthError::Invalid)) => {}
            Err(_) => panic!("expected auth invalid"),
            Ok(_) => panic!("expected auth invalid"),
        }
        match state.http_events_stream_sse(&admin_headers(), id).await {
            Err(ServerError::Auth(crate::server::AuthError::Invalid)) => {}
            Err(_) => panic!("expected auth invalid"),
            Ok(_) => panic!("expected auth invalid"),
        }
    }

    #[tokio::test]
    async fn http_events_stream_unknown_agent_returns_not_found() {
        let (state, _store) = build_state(true);
        let bogus = Uuid::new_v4();
        match state.http_events_stream(&admin_headers(), bogus).await {
            Err(ServerError::NotFound) => {}
            Err(_) => panic!("expected not found"),
            Ok(_) => panic!("expected not found"),
        }
        match state.http_events_stream_sse(&admin_headers(), bogus).await {
            Err(ServerError::NotFound) => {}
            Err(_) => panic!("expected not found"),
            Ok(_) => panic!("expected not found"),
        }
    }

    #[tokio::test]
    async fn http_usage_check_returns_accepted_and_lands_on_actor() {
        // §Usage-limits: the domain function pushes Command::UsageCheck
        // onto the actor's cmd channel. We can't observe the actual
        // scrape here (the fake actor doesn't drive a real PTY), but
        // we can assert the call doesn't error and the message lands
        // on the channel. A separate integration test
        // (warren/tests/integration.rs) wires a real actor and
        // asserts the envelope reaches the SSE stream.
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        let result = state.http_usage_check(&admin_headers(), id).await;
        assert!(
            result.is_ok(),
            "http_usage_check should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn http_usage_check_unknown_agent_returns_not_found() {
        let (state, _store) = build_state(true);
        let bogus = Uuid::new_v4();
        match state.http_usage_check(&admin_headers(), bogus).await {
            Err(ServerError::NotFound) => {}
            Err(_) => panic!("expected not found"),
            Ok(_) => panic!("expected not found"),
        }
    }

    #[tokio::test]
    async fn http_usage_check_rejects_non_admin() {
        // The endpoint is admin-only; without the admin auth grant
        // the function must surface an Auth error and never touch
        // the actor's cmd channel.
        let (state, _store) = build_state(false);
        let id = register_agent(&state);
        match state.http_usage_check(&admin_headers(), id).await {
            Err(ServerError::Auth(_)) => {}
            Err(_) => panic!("expected auth error"),
            Ok(_) => panic!("expected auth error"),
        }
    }

    // -----------------------------------------------------------------
    // §Context-window: /context endpoint mirrors /usage_check.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn http_context_check_returns_accepted_and_lands_on_actor() {
        // §Context-window: the domain function pushes
        // Command::ContextCheck onto the actor's cmd channel. We
        // can't observe the actual scrape here (the fake actor
        // doesn't drive a real PTY), but we can assert the call
        // doesn't error and the message lands on the channel. A
        // separate integration test (warren/tests/integration.rs)
        // wires a real actor and asserts the envelope reaches the
        // SSE stream.
        let (state, _store) = build_state(true);
        let id = register_agent(&state);
        let result = state.http_context_check(&admin_headers(), id).await;
        assert!(
            result.is_ok(),
            "http_context_check should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn http_context_check_unknown_agent_returns_not_found() {
        let (state, _store) = build_state(true);
        let bogus = Uuid::new_v4();
        match state.http_context_check(&admin_headers(), bogus).await {
            Err(ServerError::NotFound) => {}
            Err(_) => panic!("expected not found"),
            Ok(_) => panic!("expected not found"),
        }
    }

    #[tokio::test]
    async fn http_context_check_rejects_non_admin() {
        // §Context-window: the endpoint is admin-only; without the
        // admin auth grant the function must surface an Auth error
        // and never touch the actor's cmd channel.
        let (state, _store) = build_state(false);
        let id = register_agent(&state);
        match state.http_context_check(&admin_headers(), id).await {
            Err(ServerError::Auth(_)) => {}
            Err(_) => panic!("expected auth error"),
            Ok(_) => panic!("expected auth error"),
        }
    }
}
