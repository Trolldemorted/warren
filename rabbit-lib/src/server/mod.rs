//! Server half of the rabbit protocol — the multi-broker WebSocket
//! runtime that fans a single supervisor's term-bytes and meta-envelopes
//! out to many browser subscribers per agent, persists the event stream
//! via the [`SessionStore`] trait, and authenticates connections via
//! the [`AuthBackend`] trait.
//!
//! Modules:
//! - [`handle`] — the per-agent live handle (idle, prompt, interrupt,
//!   clear, compact, restart, resize, snapshot, terminal replay buffer)
//! - [`actor`] — the supervisor-side task that owns the rabbit WS
//!   connection and runs the per-agent prompt loop
//! - [`registry`] — the `DashMap<Uuid, AgentHandle>` shared between
//!   the rabbit WS, the browser WS, the shell WS, and the HTTP API
//! - [`ws_rabbit`] — `/ws/rabbit` (inbound supervisor connection)
//! - [`ws_browser`] — `/agent/:id/claude/ws` (browser subscriber)
//! - [`ws_shell`]  — `/agent/:id/shell/ws` (browser ↔ bash PTY)
//! - [`http`]      — `/api/agents/:id/claude/{prompt,usage,state,...}`
//!
//! Embedders construct a [`ServerState`] with concrete implementations
//! of [`SessionStore`] / [`AuthBackend`] / [`LogSink`] and call
//! [`ServerState::router`] to obtain an `axum::Router<Arc<ServerState>>`
//! that they can merge into their own larger router (see
//! `rabbit-lib/README.md` for the embedding recipe).
//!
//! See `rabbit-lib.md` §3 for the full design rationale.

use std::sync::Arc;
use uuid::Uuid;

use self::registry::AgentRegistry;

// ----- Records returned by the storage trait -----

/// A single event row returned by [`SessionStore::list_events_since`].
/// Field shape mirrors the on-disk agent_event row: the embedder's
/// `SeaOrmSessionStore` populates it from a SeaORM model; the rabbit-lib
/// HTTP layer serializes it as JSON to the API client.
#[derive(Debug, Clone)]
pub struct AgentEventRecord {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub seq: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub payload: serde_json::Value,
}

// ----- Traits external embedders must implement -----

/// Persistent log of `(agent_id, seq)` events that the actor writes as
/// it processes a supervisor's link frames. The trait is intentionally
/// tiny (three methods) so external storage backends are easy to
/// provide; the on-disk SeaORM rows are one concrete implementation.
///
/// Error model: all methods return [`StoreError`] instead of
/// `anyhow::Error`. The trait is FFI-friendly — every payload is a
/// `&str` (parsed internally) or a typed value, and every error variant
/// is structured. FFI consumers (PyO3 / napi-rs / cgo) can pattern-match
/// the variant without unwrapping a stringly-typed `anyhow::Error`.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync + 'static {
    /// Returns the next free `seq` for `agent_id`'s event log. The
    /// store is responsible for persisting this on insert; the caller
    /// uses it as the dedup watermark and to populate
    /// `(agent_id, seq)` rows.
    async fn next_event_seq(&self, agent_id: Uuid) -> Result<i64, StoreError>;

    /// Append one event to `agent_id`'s log at `seq`. `payload_json`
    /// is a serialized `serde_json::Value` (a `&str` the trait parses
    /// internally) — using a `&str` keeps the trait `dyn`-safe and
    /// FFI-friendly: foreign bindings can hand over a raw string
    /// without needing to construct a Rust `Value`. Returns
    /// [`StoreError::Duplicate`] when the `(agent_id, seq)` pair is
    /// already persisted; the actor swallows that variant as "already
    /// persisted" via `.ok()`. FFI consumers wanting to skip the parse
    /// can call [`SessionStore::insert_event_value`] instead.
    async fn insert_event(
        &self,
        agent_id: Uuid,
        seq: i64,
        kind: &str,
        payload_json: &str,
    ) -> Result<(), StoreError>;

    /// Source-compatible overload of [`SessionStore::insert_event`]
    /// that takes a pre-parsed [`serde_json::Value`] directly. Useful
    /// when the caller already holds a `Value` and wants to skip the
    /// serialization round trip.
    async fn insert_event_value(
        &self,
        agent_id: Uuid,
        seq: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<(), StoreError>;

    /// Return up to `limit` events for `agent_id` with `seq > since`,
    /// ordered ascending by `seq`. Used by the HTTP
    /// `claude_events` endpoint to back the event-log UI.
    async fn list_events_since(
        &self,
        agent_id: Uuid,
        since: i64,
        limit: u64,
    ) -> Result<Vec<AgentEventRecord>, StoreError>;
}

/// Errors a [`SessionStore`] can surface. The variants are deliberately
/// structured so embedders and FFI bindings can route on them without
/// having to parse a stringly-typed `anyhow::Error`. New variants
/// should be additive — embedders implementing the trait may pattern-
/// match on a non-exhaustive list and we want them to keep compiling
/// when we add a new variant.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The underlying storage backend is unreachable, the connection
    /// is broken, or a similar transient transport failure. Callers
    /// may retry.
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),
    /// The request itself is malformed (e.g. payload_json is not
    /// valid JSON, or `seq` is negative). Not retriable.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// The `(agent_id, seq)` pair was already persisted. The actor
    /// swallows this variant as "already persisted"; surfacing it as a
    /// distinct variant lets embedders log duplicates without
    /// classifying them as a backend failure.
    #[error("duplicate seq (already persisted)")]
    Duplicate,
    /// Catch-all for everything else. Embedders should map their
    /// backend's native errors here. The boxed `dyn Error` carries the
    /// original error chain for logging.
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl From<anyhow::Error> for StoreError {
    fn from(e: anyhow::Error) -> Self {
        StoreError::Other(Box::new(std::io::Error::other(e.to_string())))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Invalid(format!("invalid JSON payload: {e}"))
    }
}

/// Auth surface for inbound connections: rabbit (agent-token) and
/// browser (admin-session-cookie). The trait returns the authenticated
/// identity on success or a structured [`AuthError`] on failure.
///
/// `headers` is the framework-agnostic [`::http::HeaderMap`] (the
/// standalone `http` crate — NOT `axum::http`, which is just a
/// re-export of the same type). Using the standalone crate means
/// embedders using hyper / actix / tokio-tungstenite / FFI bindings
/// can pass their own header bag without depending on axum. Two
/// string-keyed overloads (`authenticate_*_from_strings`) accept the
/// cookie + authorization values directly — those are the entry points
/// FFI consumers (PyO3 / napi-rs / cgo) call, since they rarely have
/// a `HeaderMap` handy. The default impls of the string overloads
/// build a `HeaderMap` and delegate to the typed methods, so existing
/// embedders don't have to implement them.
#[async_trait::async_trait]
pub trait AuthBackend: Send + Sync + 'static {
    /// Validate the `Authorization: Bearer …` header against the
    /// agent-token table. Returns the authenticated `agent_id` on
    /// success.
    async fn authenticate_agent(&self, headers: &::http::HeaderMap) -> Result<Uuid, AuthError>;

    /// Validate the session cookie / header for admin endpoints.
    /// Returns `true` iff the caller is an authenticated admin.
    async fn authenticate_admin(&self, headers: &::http::HeaderMap) -> Result<bool, AuthError>;

    /// FFI-friendly overload of [`Self::authenticate_agent`]. `cookie`
    /// is the literal value of the session cookie (or `None`), and
    /// `authorization` is the literal `Authorization` header value
    /// (or `None`). Default impl constructs a [`::http::HeaderMap`]
    /// and delegates.
    async fn authenticate_agent_from_strings(
        &self,
        authorization: Option<&str>,
        cookie: Option<&str>,
    ) -> Result<Uuid, AuthError> {
        let headers = build_header_map(authorization, cookie);
        self.authenticate_agent(&headers).await
    }

    /// FFI-friendly overload of [`Self::authenticate_admin`]. See
    /// [`Self::authenticate_agent_from_strings`] for parameter
    /// semantics.
    async fn authenticate_admin_from_strings(
        &self,
        authorization: Option<&str>,
        cookie: Option<&str>,
    ) -> Result<bool, AuthError> {
        let headers = build_header_map(authorization, cookie);
        self.authenticate_admin(&headers).await
    }
}

/// Build a [`::http::HeaderMap`] from raw cookie / authorization strings.
/// Used by the default impls of `authenticate_*_from_strings`. Malformed
/// `Authorization` values are dropped (callers can string-skip
/// validation; the trait body re-parses the prefix via
/// `header::AUTHORIZATION` anyway).
fn build_header_map(authorization: Option<&str>, cookie: Option<&str>) -> ::http::HeaderMap {
    use ::http::header::{HeaderName, HeaderValue};
    let mut headers = ::http::HeaderMap::new();
    if let Some(value) = authorization.and_then(|s| HeaderValue::from_str(s).ok()) {
        headers.insert(HeaderName::from_static("authorization"), value);
    }
    if let Some(value) = cookie.and_then(|s| HeaderValue::from_str(s).ok()) {
        headers.insert(HeaderName::from_static("cookie"), value);
    }
    headers
}

/// Reasons the auth trait can reject a request. War maps this onto its
/// own `AppError::Unauthorized` via a `From` impl in the embedder
/// adapter.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or malformed authorization header")]
    Missing,
    #[error("invalid credentials")]
    Invalid,
    #[error("internal auth failure: {0}")]
    Internal(String),
}

/// axum-compatible error type returned by the lib's HTTP handlers and
/// the framework-agnostic `http_*` domain functions. Wraps
/// [`AuthError`] (for the auth gate), [`StoreError`] (for storage
/// failures), and `anyhow::Error` (for anything else).
/// `IntoResponse` maps each variant to the right status code so the
/// embedder doesn't have to write a per-handler conversion.
///
/// Phase 3 lifts this to `pub` because the `http_api` module returns
/// `ServerResult<T>` from public methods; embedders must be able to
/// pattern-match on the variants. The `IntoResponse` impl lives here
/// until Phase 5 moves it to the `rabbit-lib-axum` adapter crate.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenience alias for the lib's HTTP handler return type.
pub type ServerResult<T> = std::result::Result<T, ServerError>;

/// Surface for the actor's `log` calls. Default impl is `log::log!`.
/// Embedders can swap in a structured logger.
pub trait LogSink: Send + Sync + 'static {
    fn log(&self, level: log::Level, target: &str, message: &str);
}

/// Default `LogSink` that just forwards to the `log` crate's macros.
pub struct StdLogSink;

impl LogSink for StdLogSink {
    fn log(&self, level: log::Level, target: &str, message: &str) {
        log::log!(target: target, level, "{}", message);
    }
}

#[cfg(test)]
mod auth_backend_tests {
    //! Pin the FFI string overloads' delegation contract: a
    //! `HeaderMap`-only implementor should accept traffic from FFI
    //! consumers via the default `*_from_strings` impls, and unparseable
    //! inputs should produce `AuthError::Missing` (not a panic).
    use super::*;

    /// Stub auth that ignores cookies, only accepts a fixed bearer
    /// token, and records the last `Authorization` header value it saw.
    /// Lets us verify the string overloads actually route through the
    /// HeaderMap-based primary methods.
    struct StubAuth {
        last_authorization: std::sync::Mutex<Option<String>>,
    }

    impl StubAuth {
        fn new() -> Self {
            Self {
                last_authorization: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl AuthBackend for StubAuth {
        async fn authenticate_agent(&self, headers: &::http::HeaderMap) -> Result<Uuid, AuthError> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            *self.last_authorization.lock().unwrap() = auth.clone();
            if auth.as_deref() == Some("Bearer good-token") {
                Ok(Uuid::nil())
            } else {
                Err(AuthError::Missing)
            }
        }

        async fn authenticate_admin(
            &self,
            _headers: &::http::HeaderMap,
        ) -> Result<bool, AuthError> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn string_overload_delegates_to_header_map() {
        let stub = StubAuth::new();
        // Use the FFI overload directly. Should produce a HeaderMap
        // with `authorization: Bearer good-token` and call into
        // `authenticate_agent`, returning the nil Uuid.
        let id = stub
            .authenticate_agent_from_strings(Some("Bearer good-token"), None)
            .await
            .expect("valid");
        assert_eq!(id, Uuid::nil());
        let recorded = stub.last_authorization.lock().unwrap().clone();
        assert_eq!(recorded.as_deref(), Some("Bearer good-token"));
    }

    #[tokio::test]
    async fn string_overload_missing_creds_returns_false() {
        // Neither header nor cookie. The stub's `authenticate_admin`
        // returns `Ok(false)` (NOT `Missing`) for a missing header — the
        // overload must delegate faithfully to the typed method.
        let stub = StubAuth::new();
        let ok = stub
            .authenticate_admin_from_strings(None, None)
            .await
            .expect("admin query");
        assert!(!ok);
    }

    #[tokio::test]
    async fn string_overload_unparseable_header_value_drops_silently() {
        // Header values that violate HTTP semantics (e.g. containing a
        // raw newline) cannot be represented in a HeaderMap. The
        // default overload must drop them rather than panic — FFI
        // consumers shouldn't crash on hostile input.
        let stub = StubAuth::new();
        let result = stub
            .authenticate_agent_from_strings(Some("bad\nvalue"), Some("warren_session=abc"))
            .await;
        // The auth string is dropped, the cookie is preserved. With
        // no `Authorization` header the stub returns Missing.
        assert!(matches!(result, Err(AuthError::Missing)));
    }
}

#[cfg(test)]
mod store_error_tests {
    //! Pin Phase 4's FFI-friendly `StoreError` + `&str` payload shape:
    //! the error variant round-trips, `insert_event` parses a JSON
    //! string into a `Value`, `insert_event_value` accepts a pre-parsed
    //! `Value`, and bad JSON surfaces as `Invalid`.
    use super::*;

    struct CapturingStore {
        last_payload_json: std::sync::Mutex<Option<String>>,
        last_payload_value: std::sync::Mutex<Option<serde_json::Value>>,
    }

    impl CapturingStore {
        fn new() -> Self {
            Self {
                last_payload_json: std::sync::Mutex::new(None),
                last_payload_value: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionStore for CapturingStore {
        async fn next_event_seq(&self, _agent_id: Uuid) -> Result<i64, StoreError> {
            Ok(1)
        }
        async fn insert_event(
            &self,
            _agent_id: Uuid,
            _seq: i64,
            _kind: &str,
            payload_json: &str,
        ) -> Result<(), StoreError> {
            let parsed: serde_json::Value = serde_json::from_str(payload_json)?;
            *self.last_payload_json.lock().unwrap() = Some(payload_json.to_string());
            *self.last_payload_value.lock().unwrap() = Some(parsed);
            Ok(())
        }
        async fn insert_event_value(
            &self,
            _agent_id: Uuid,
            _seq: i64,
            _kind: &str,
            payload: serde_json::Value,
        ) -> Result<(), StoreError> {
            *self.last_payload_value.lock().unwrap() = Some(payload);
            Ok(())
        }
        async fn list_events_since(
            &self,
            _agent_id: Uuid,
            _since: i64,
            _limit: u64,
        ) -> Result<Vec<AgentEventRecord>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn store_insert_event_parses_json_string() {
        let store = CapturingStore::new();
        let id = Uuid::new_v4();
        store
            .insert_event(id, 1, "kind", r#"{"a":1,"b":"two"}"#)
            .await
            .expect("ok");
        let captured = store.last_payload_value.lock().unwrap().clone();
        assert_eq!(captured, Some(serde_json::json!({"a": 1, "b": "two"})));
    }

    #[tokio::test]
    async fn store_insert_event_invalid_json_returns_invalid() {
        let store = CapturingStore::new();
        let id = Uuid::new_v4();
        let result = store.insert_event(id, 1, "kind", "not json").await;
        match result {
            Err(StoreError::Invalid(_)) => {}
            other => panic!("expected StoreError::Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn store_insert_event_value_skips_parse() {
        let store = CapturingStore::new();
        let id = Uuid::new_v4();
        let payload = serde_json::json!({"k": [1, 2, 3]});
        store
            .insert_event_value(id, 1, "kind", payload.clone())
            .await
            .expect("ok");
        let captured = store.last_payload_value.lock().unwrap().clone();
        assert_eq!(captured, Some(payload));
        // The string overload was not called, so the json-string field
        // stays None.
        assert!(store.last_payload_json.lock().unwrap().is_none());
    }

    #[test]
    fn store_error_from_anyhow_preserves_message() {
        // Phase 4 keeps `From<anyhow::Error> for StoreError` so callers
        // that historically returned `anyhow::Error` continue to compile
        // while the variant the lib uses internally is the structured
        // `StoreError::*`.
        let original = anyhow::anyhow!("backend connection reset");
        let store_err: StoreError = original.into();
        match store_err {
            StoreError::Other(_) => {}
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn store_error_display_includes_variant_payload() {
        // The Display impls are part of the FFI surface — log lines and
        // error toString() calls reach across the binding boundary and
        // embedders rely on the human-readable string.
        assert_eq!(
            StoreError::Duplicate.to_string(),
            "duplicate seq (already persisted)"
        );
        assert_eq!(
            StoreError::Unavailable("conn refused".into()).to_string(),
            "storage backend unavailable: conn refused"
        );
    }
}

// ----- ServerState (the lib's analogue of warren's AppState) -----

/// The state type the lib's domain methods take. Embedders construct it
/// once, hand it to the `rabbit-lib-axum` adapter (or call the
/// `http_*` domain methods / `ws_*::handle` functions directly from a
/// custom transport integration).
pub struct ServerState {
    pub registry: AgentRegistry,
    pub store: Arc<dyn SessionStore>,
    pub auth: Arc<dyn AuthBackend>,
    pub log_sink: Arc<dyn LogSink>,
    /// §Simplify TUI sizing: the static grid size warren advertises to
    /// each rabbit on connect. `None` falls back to (120, 40) at the
    /// actor. Embedders that want a different grid set this; most leave
    /// it `None`.
    pub tui_size: Option<(u16, u16)>,
}

#[doc(hidden)]
pub mod mock {
    //! Test-only mocks for the trait surface. Used by integration
    //! tests and examples that need a `ServerState` without a real
    //! Postgres / auth backend. Marked `#[doc(hidden)]` so embedders
    //! building production systems don't pick it up; it's intended
    //! for `#[cfg(test)]` and `examples/` usage.

    use super::*;
    use std::sync::Mutex;

    /// Auth backend that accepts every request. Records the last
    /// `agent_id` it was asked to authenticate, so tests can assert
    /// which token reached it.
    pub struct AlwaysAllowAuth {
        pub last_agent: Mutex<Option<uuid::Uuid>>,
    }

    impl Default for AlwaysAllowAuth {
        fn default() -> Self {
            Self {
                last_agent: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl AuthBackend for AlwaysAllowAuth {
        async fn authenticate_agent(
            &self,
            _headers: &::http::HeaderMap,
        ) -> Result<uuid::Uuid, AuthError> {
            let id = uuid::Uuid::new_v4();
            *self.last_agent.lock().unwrap() = Some(id);
            Ok(id)
        }
        async fn authenticate_admin(
            &self,
            _headers: &::http::HeaderMap,
        ) -> Result<bool, AuthError> {
            Ok(true)
        }
    }

    /// Session store that records each `insert_event` call and returns
    /// a monotonically-increasing seq. Tests inspect `inserted` to
    /// verify the actor's persistence flow.
    pub struct MockSessionStore {
        pub next_seq: Mutex<i64>,
        pub inserted: Mutex<Vec<(uuid::Uuid, i64, String)>>,
    }

    impl Default for MockSessionStore {
        fn default() -> Self {
            Self {
                next_seq: Mutex::new(1),
                inserted: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionStore for MockSessionStore {
        async fn next_event_seq(&self, _agent_id: uuid::Uuid) -> Result<i64, StoreError> {
            let mut s = self.next_seq.lock().unwrap();
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
            self.inserted
                .lock()
                .unwrap()
                .push((agent_id, seq, kind.to_string()));
            Ok(())
        }
        async fn insert_event_value(
            &self,
            agent_id: uuid::Uuid,
            seq: i64,
            kind: &str,
            _payload: serde_json::Value,
        ) -> Result<(), StoreError> {
            self.inserted
                .lock()
                .unwrap()
                .push((agent_id, seq, kind.to_string()));
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
}

// Server modules — the implementation that owns the per-agent live
// handles and the WebSocket / HTTP surfaces that front the supervisor
// half of the lib.
//
// `registry` stays pub because embedders construct and reference
// `AgentRegistry` (warren's `/agent/:id/claude/history` route takes
// `&AgentRegistry`). The other five are crate-internal: their public
// surfaces are reached only through `ServerState::router`, which is
// what embedders actually wire up.
pub mod registry;

pub mod transport;

pub use transport::{CloseReason, DynWsTransport, TransportMsg, WsTransport};

pub(crate) mod actor;
pub mod handle;
pub mod http_api;
pub mod ws_browser;
pub mod ws_rabbit;
pub mod ws_shell;
