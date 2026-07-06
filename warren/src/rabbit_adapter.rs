//! Adapters that wire Warren's SeaORM data layer + admin/agent auth into
//! the trait surface that `rabbit-lib::server` consumes. This is the only
//! place that needs to know about both sides; the rest of warren just
//! builds a `ServerState` with these two pieces and hands it to
//! `state.live.router()`.
//!
//! Each adapter is intentionally thin: it translates between the lib's
//! `anyhow::Error` shape and Warren's `AppError`-flavored SQL/lookup
//! calls, but it does NOT introduce new persistence behaviour. If you
//! need a new query, add it to `db_ops` and call it from here.

use crate::auth::{lookup_agent_by_token, validate_admin_session};
use crate::db::Db;
use crate::entity::agent_event;
use rabbit_lib::server::{AgentEventRecord, AuthBackend, AuthError, SessionStore};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use std::sync::Arc;
use uuid::Uuid;

/// SeaORM-backed implementation of [`SessionStore`]. Persists
/// `agent_event` rows in Postgres and delegates seq allocation to
/// `db_ops::next_event_seq`.
pub struct SeaOrmSessionStore {
    db: Db,
}

impl SeaOrmSessionStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

#[async_trait::async_trait]
impl SessionStore for SeaOrmSessionStore {
    async fn next_event_seq(&self, agent_id: Uuid) -> anyhow::Result<i64> {
        crate::db_ops::next_event_seq(&self.db, agent_id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    async fn insert_event(
        &self,
        agent_id: Uuid,
        seq: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<()> {
        let id = Uuid::new_v4();
        let am = agent_event::ActiveModel {
            id: Set(id),
            agent_id: Set(agent_id),
            seq: Set(seq),
            kind: Set(kind.to_string()),
            payload: Set(payload),
            ..Default::default()
        };
        am.insert(&self.db).await?;
        Ok(())
    }

    async fn list_events_since(
        &self,
        agent_id: Uuid,
        since_seq: i64,
        limit: u64,
    ) -> anyhow::Result<Vec<AgentEventRecord>> {
        let rows = agent_event::Entity::find()
            .filter(agent_event::Column::AgentId.eq(agent_id))
            .filter(agent_event::Column::Seq.gt(since_seq))
            .order_by_asc(agent_event::Column::Seq)
            .limit(Some(limit))
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|m| AgentEventRecord {
                id: m.id,
                agent_id: m.agent_id,
                seq: m.seq,
                ts: m.ts,
                kind: m.kind,
                payload: m.payload,
            })
            .collect())
    }
}

/// Warren-flavored auth: admin session cookie OR `Authorization: Bearer
/// <admin-or-agent-token>`. Returns the authenticated agent's id for
/// `authenticate_agent`; a boolean for `authenticate_admin`. Mirrors the
/// precedence the in-process `auth::AuthContext` extractor uses.
pub struct WarAuthBackend {
    db: Db,
}

impl WarAuthBackend {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Look for a session cookie first (admin), then fall back to
    /// bearer-token auth (admin or agent). Used by the trait methods to
    /// share the precedence logic.
    async fn classify(&self, headers: &axum::http::HeaderMap) -> Result<AdminOrAgent, AuthError> {
        use axum::http::header;
        if let Some(cookie) = crate::auth::read_session_cookie(headers) {
            if validate_admin_session(&self.db, &cookie)
                .await
                .map_err(|e| AuthError::Internal(e.to_string()))?
            {
                return Ok(AdminOrAgent::Admin);
            }
        }
        if let Some(token) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        {
            if validate_admin_session(&self.db, token)
                .await
                .map_err(|e| AuthError::Internal(e.to_string()))?
            {
                return Ok(AdminOrAgent::Admin);
            }
            if let Some(agent) = lookup_agent_by_token(&self.db, token)
                .await
                .map_err(|e| AuthError::Internal(e.to_string()))?
            {
                return Ok(AdminOrAgent::Agent(agent.id));
            }
        }
        Err(AuthError::Missing)
    }
}

enum AdminOrAgent {
    Admin,
    Agent(Uuid),
}

#[async_trait::async_trait]
impl AuthBackend for WarAuthBackend {
    async fn authenticate_agent(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<Uuid, AuthError> {
        match self.classify(headers).await? {
            AdminOrAgent::Agent(id) => Ok(id),
            AdminOrAgent::Admin => Err(AuthError::Invalid),
        }
    }

    async fn authenticate_admin(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<bool, AuthError> {
        match self.classify(headers).await? {
            AdminOrAgent::Admin => Ok(true),
            AdminOrAgent::Agent(_) => Ok(false),
        }
    }
}

/// Sugar for the construction site in `main.rs` — keeps the call to
/// one line and centralises the `Arc::new` dance.
pub fn build_server_state(db: Db) -> Arc<rabbit_lib::server::ServerState> {
    let store: Arc<dyn SessionStore> = Arc::new(SeaOrmSessionStore::new(db.clone()));
    let auth: Arc<dyn AuthBackend> = Arc::new(WarAuthBackend::new(db.clone()));
    let log_sink: Arc<dyn rabbit_lib::server::LogSink> =
        Arc::new(rabbit_lib::server::StdLogSink);
    Arc::new(rabbit_lib::server::ServerState {
        registry: rabbit_lib::server::new_registry(),
        store,
        auth,
        log_sink,
    })
}
