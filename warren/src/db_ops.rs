use crate::db::Db;
use crate::entity::{agent, channel, request};
use crate::error::{map_unique_conflict, AppError, AppResult};
use crate::models::{AgentNew, AgentPatch, ChannelNew, ChannelPatch, RequestNew};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, FromQueryResult,
    IntoActiveModel, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};
use serde_json::Value;
use uuid::Uuid;

pub async fn list_agents(db: &Db) -> AppResult<Vec<agent::Model>> {
    Ok(agent::Entity::find()
        .order_by_desc(agent::Column::CreatedAt)
        .all(db)
        .await?)
}

pub async fn get_agent(db: &Db, id: Uuid) -> AppResult<Option<agent::Model>> {
    Ok(agent::Entity::find_by_id(id).one(db).await?)
}

pub async fn create_agent(db: &Db, new: &AgentNew) -> AppResult<agent::Model> {
    let authtoken = crate::ids::new_agent_token();
    insert_agent(db, new, &authtoken).await
}

pub async fn insert_agent(db: &Db, new: &AgentNew, authtoken: &str) -> AppResult<agent::Model> {
    let am = agent::ActiveModel {
        name: Set(new.name.clone()),
        class: Set(new.class.clone()),
        kind: Set(new.kind.clone()),
        model: Set(new.model.clone()),
        prompt: Set(new.prompt.clone()),
        authtoken: Set(authtoken.to_string()),
        ..Default::default()
    };
    match am.insert(db).await {
        Ok(m) => Ok(m),
        Err(e) => Err(map_unique_conflict(e, "agent name or token already exists")),
    }
}

pub async fn update_agent(db: &Db, id: Uuid, patch: &AgentPatch) -> AppResult<()> {
    let mut am = agent::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(AppError::NotFound)?
        .into_active_model();
    if let Some(n) = &patch.name {
        am.name = Set(n.clone());
    }
    if let Some(c) = &patch.class {
        am.class = Set(c.clone());
    }
    if let Some(k) = &patch.kind {
        am.kind = Set(k.clone());
    }
    if let Some(m) = &patch.model {
        am.model = Set(m.clone());
    }
    if let Some(p) = &patch.prompt {
        am.prompt = Set(p.clone());
    }
    am.update(db).await?;
    Ok(())
}

pub async fn patch_agent(db: &Db, id: Uuid, patch: &AgentPatch) -> AppResult<agent::Model> {
    update_agent(db, id, patch).await?;
    get_agent(db, id).await?.ok_or(AppError::NotFound)
}

pub async fn delete_agent(db: &Db, id: Uuid) -> AppResult<()> {
    let res = agent::Entity::delete_by_id(id).exec(db).await?;
    if res.rows_affected == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn create_request(
    db: &Db,
    new: &RequestNew,
    initial_status: i16,
    sender_agent_id: Option<Uuid>,
) -> AppResult<request::Model> {
    let am = request::ActiveModel {
        target_class: Set(new.target_class.clone()),
        target_type: Set(new.target_type.clone()),
        payload: Set(new.payload.clone()),
        status: Set(initial_status),
        sender_agent_id: Set(sender_agent_id),
        channel_id: Set(new.channel_id),
        ..Default::default()
    };
    Ok(am.insert(db).await?)
}

pub async fn list_all_requests(
    db: &Db,
    status_filter: Option<i16>,
    limit: u64,
    offset: u64,
) -> AppResult<Vec<request::Model>> {
    let mut q = request::Entity::find().order_by_desc(request::Column::CreatedAt);
    if let Some(s) = status_filter {
        q = q.filter(request::Column::Status.eq(s));
    }
    Ok(q.limit(Some(limit)).offset(Some(offset)).all(db).await?)
}

/// All requests an agent should see at `/api/requests`:
/// - sent by them (regardless of status, unless `include_acknowledged = false`)
/// - claimable now (matching class+kind, pending, unclaimed)
/// - claimed by them (any status where they hold the claim)
/// - responded by them (response set, may still be waiting for admin approval)
pub async fn list_requests_for_agent(
    db: &Db,
    agent_id: Uuid,
    class: &str,
    kind: Option<&str>,
    include_acknowledged: bool,
) -> AppResult<Vec<request::Model>> {
    let target_type_sql = match kind {
        Some(k) => format!("'{k}'"),
        None => "NULL".to_string(),
    };
    let pending = request::AWAITING_RESPONSE;
    let ack = request::ACKNOWLEDGED;
    let sent_branch = if include_acknowledged {
        format!("sender_agent_id = '{agent_id}'")
    } else {
        format!("sender_agent_id = '{agent_id}' AND status <> {ack}")
    };
    let claimed_branch = if include_acknowledged {
        format!("claimed_by = '{agent_id}'")
    } else {
        format!("claimed_by = '{agent_id}' AND status <> {ack}")
    };
    let sql = format!(
        "SELECT id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, channel_id, created_at, responded_at, acknowledged_at \
           FROM requests \
          WHERE {sent_branch} \
             OR (status = {pending} \
                 AND claimed_by IS NULL \
                 AND target_class = '{class}' \
                 AND (target_type IS NULL OR target_type = {target_type_sql})) \
             OR {claimed_branch} \
          ORDER BY created_at ASC"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let rows = db.query_all(stmt).await?;
    rows.into_iter()
        .map(|r| request::Model::from_query_result(&r, ""))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::from)
}

pub async fn get_request(db: &Db, id: Uuid) -> AppResult<Option<request::Model>> {
    Ok(request::Entity::find_by_id(id).one(db).await?)
}

pub async fn delete_request(db: &Db, id: Uuid) -> AppResult<()> {
    let res = request::Entity::delete_by_id(id).exec(db).await?;
    if res.rows_affected == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn claim_request(db: &Db, id: Uuid, agent_id: Uuid) -> AppResult<request::Model> {
    let pending = request::AWAITING_RESPONSE;
    let sql = format!(
        "UPDATE requests SET claimed_by = '{agent_id}', claimed_at = NOW() WHERE id = '{id}' AND status = {pending} AND claimed_by IS NULL AND target_class = (SELECT class FROM agents WHERE id = '{agent_id}') AND (target_type IS NULL OR target_type = (SELECT kind FROM agents WHERE id = '{agent_id}')) RETURNING id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, channel_id, created_at, responded_at, acknowledged_at"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let row = db.query_one(stmt).await?;
    match row {
        Some(r) => Ok(request::Model::from_query_result(&r, "")?),
        None => Err(AppError::Conflict(
            "request not in inbox or already claimed".into(),
        )),
    }
}

pub async fn respond_to_request(
    db: &Db,
    id: Uuid,
    agent_id: Uuid,
    response: &Value,
) -> AppResult<request::Model> {
    let response_json = serde_json::to_string(response).unwrap_or_else(|_| "null".to_string());
    let pending = request::AWAITING_RESPONSE;
    let sql = format!(
        "UPDATE requests SET response = '{response_json}'::jsonb, responded_at = NOW() WHERE id = '{id}' AND claimed_by = '{agent_id}' AND status = {pending} RETURNING id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, channel_id, created_at, responded_at, acknowledged_at"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let row = db.query_one(stmt).await?;
    match row {
        Some(r) => Ok(request::Model::from_query_result(&r, "")?),
        None => Err(AppError::Conflict("not claimed by this agent".into())),
    }
}

pub async fn set_request_status(db: &Db, id: Uuid, from: i16, to: i16) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(request::Column::Status, Expr::value(to))
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(from))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict(format!(
            "request not in status {}",
            request::status_label(from)
        )));
    }
    Ok(())
}

pub async fn accept_request_response(db: &Db, id: Uuid) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(request::Column::Status, Expr::value(request::DONE))
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::AWAITING_RESPONSE))
        .filter(request::Column::Response.is_not_null())
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict("no responded request to accept".into()));
    }
    Ok(())
}

pub async fn reject_request_response(db: &Db, id: Uuid) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(request::Column::Response, Expr::value(Value::Null))
        .col_expr(
            request::Column::ClaimedBy,
            Expr::value(None as Option<Uuid>),
        )
        .col_expr(
            request::Column::ClaimedAt,
            Expr::value(None as Option<chrono::DateTime<chrono::Utc>>),
        )
        .col_expr(
            request::Column::RespondedAt,
            Expr::value(None as Option<chrono::DateTime<chrono::Utc>>),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::AWAITING_RESPONSE))
        .filter(request::Column::Response.is_not_null())
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict("no response to reject".into()));
    }
    Ok(())
}

/// Mark a `done` request as acknowledged by an agent (or by an admin on
/// the agent's behalf). Sets `status = ACKNOWLEDGED` and stamps
/// `acknowledged_at`. Atomic; if the row is not in `done` state, or
/// `by_admin = false` and the caller is neither sender nor claimer,
/// returns `Conflict`.
pub async fn acknowledge_request(
    db: &Db,
    id: Uuid,
    caller_id: Uuid,
    by_admin: bool,
) -> AppResult<request::Model> {
    let owner_clause = if by_admin {
        String::new()
    } else {
        format!(" AND (sender_agent_id = '{caller_id}' OR claimed_by = '{caller_id}')")
    };
    let done = request::DONE;
    let ack = request::ACKNOWLEDGED;
    let sql = format!(
        "UPDATE requests SET status = {ack}, acknowledged_at = NOW() \
         WHERE id = '{id}' AND status = {done}{owner_clause} \
         RETURNING id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, channel_id, created_at, responded_at, acknowledged_at"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let row = db.query_one(stmt).await?;
    match row {
        Some(r) => Ok(request::Model::from_query_result(&r, "")?),
        None => Err(AppError::Conflict(
            "request not done yet or not owned by caller".into(),
        )),
    }
}

/// Admin-only: revert an `acknowledged` request back to `done`.
pub async fn unacknowledge_request(db: &Db, id: Uuid) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(request::Column::Status, Expr::value(request::DONE))
        .col_expr(
            request::Column::AcknowledgedAt,
            Expr::value(None as Option<chrono::DateTime<chrono::Utc>>),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::ACKNOWLEDGED))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict("request not acknowledged".into()));
    }
    Ok(())
}

pub async fn delete_admin_session(db: &Db, token: &str) -> AppResult<()> {
    use crate::entity::admin_session;
    let _ = admin_session::Entity::delete_many()
        .filter(admin_session::Column::Token.eq(token.to_string()))
        .exec(db)
        .await?;
    Ok(())
}

#[derive(FromQueryResult)]
struct AgentClassRow {
    class: String,
}

#[derive(FromQueryResult)]
struct AgentKindRow {
    kind: Option<String>,
}

pub async fn distinct_agent_classes(db: &Db) -> AppResult<Vec<String>> {
    let rows = AgentClassRow::find_by_statement(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT DISTINCT class FROM agents ORDER BY class".to_string(),
    ))
    .all(db)
    .await?;
    Ok(rows.into_iter().map(|r| r.class).collect())
}

pub async fn distinct_agent_kinds(db: &Db) -> AppResult<Vec<String>> {
    let rows = AgentKindRow::find_by_statement(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT DISTINCT kind FROM agents WHERE kind IS NOT NULL ORDER BY kind".to_string(),
    ))
    .all(db)
    .await?;
    Ok(rows.into_iter().filter_map(|r| r.kind).collect())
}

#[derive(FromQueryResult)]
pub struct MigrationRow {
    pub version: String,
    pub description: Option<String>,
    pub applied_at: Option<String>,
}

#[derive(FromQueryResult)]
struct ColumnInfoRow {
    table_schema: String,
    table_name: String,
    column_name: String,
    data_type: String,
}

const TS_FMT: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// Locate the revision table and its timestamp column, returning
/// (qualified_table, column_name, data_type).
async fn discover_revision_column(
    db: &Db,
    schemas: &[&str],
    column_candidates: &[&str],
) -> AppResult<Option<(String, String, String)>> {
    // schemata/columns come from a hardcoded allowlist (no user input), so
    // string-formatting into ARRAY[...] is safe.
    let schema_arr = format!(
        "ARRAY[{}]",
        schemas
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",")
    );
    let table_arr = "ARRAY['atlas_schema_revisions','schema_migrations','public']".to_string();
    let col_arr = format!(
        "ARRAY[{}]",
        column_candidates
            .iter()
            .map(|c| format!("'{}'", c.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",")
    );
    let sql = format!(
        "SELECT table_schema, table_name, column_name, data_type \
           FROM information_schema.columns \
          WHERE table_schema = ANY({schema_arr}) \
            AND table_name = ANY({table_arr}::text[]) \
            AND column_name = ANY({col_arr})"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let rows: Vec<ColumnInfoRow> = ColumnInfoRow::find_by_statement(stmt).all(db).await?;
    for schema in schemas {
        for col in column_candidates {
            if let Some(r) = rows
                .iter()
                .find(|r| r.table_schema == *schema && r.column_name == *col)
            {
                let qualified = format!("{}.{}", r.table_schema, r.table_name);
                return Ok(Some((
                    qualified,
                    r.column_name.clone(),
                    r.data_type.clone(),
                )));
            }
        }
    }
    Ok(None)
}

pub async fn list_migrations(db: &Db) -> AppResult<Vec<MigrationRow>> {
    // 1) Atlas — modern `executed_at TIMESTAMPTZ` is the canonical timestamp.
    //    Older atlases used `applied_at BIGINT` (nanoseconds) or
    //    `applied TIMESTAMPTZ`. `applied` (bigint count) is NOT a timestamp.
    let schemas = ["atlas_schema_revisions", "public"];
    let columns = ["executed_at", "applied_at", "applied"];
    let info = discover_revision_column(db, &schemas, &columns).await?;
    if let Some((table, col, data_type)) = info {
        return query_atlas_migrations(db, &table, &col, &data_type).await;
    }
    // 2) golang-migrate / sqlx-style fallback.
    let info = discover_revision_column(db, &["public"], &["installed_on"]).await?;
    if let Some((table, col, data_type)) = info {
        return query_schema_migrations(db, &table, &col, &data_type).await;
    }
    Ok(vec![])
}

async fn query_atlas_migrations(
    db: &Db,
    table: &str,
    col: &str,
    data_type: &str,
) -> AppResult<Vec<MigrationRow>> {
    let sql = match data_type {
        "bigint" | "integer" | "smallint" => format!(
            "SELECT version::text AS version, \
                    description, \
                    to_char(timezone('UTC', to_timestamp({col}::double precision / 1e9)), {TS_FMT}) AS applied_at \
               FROM {table} \
              ORDER BY version"
        ),
        "timestamp with time zone" | "timestamp without time zone" => format!(
            "SELECT version::text AS version, \
                    description, \
                    to_char(timezone('UTC', {col}), {TS_FMT}) AS applied_at \
               FROM {table} \
              ORDER BY version"
        ),
        _ => return Ok(vec![]),
    };
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    Ok(MigrationRow::find_by_statement(stmt).all(db).await?)
}

async fn query_schema_migrations(
    db: &Db,
    table: &str,
    col: &str,
    data_type: &str,
) -> AppResult<Vec<MigrationRow>> {
    let sql = match data_type {
        "bigint" | "integer" | "smallint" => format!(
            "SELECT version::text AS version, \
                    NULL::text AS description, \
                    to_char(timezone('UTC', to_timestamp({col}::double precision / 1e9)), {TS_FMT}) AS applied_at \
               FROM {table} \
              ORDER BY version"
        ),
        "timestamp with time zone" | "timestamp without time zone" => format!(
            "SELECT version::text AS version, \
                    NULL::text AS description, \
                    to_char(timezone('UTC', {col}), {TS_FMT}) AS applied_at \
               FROM {table} \
              ORDER BY version"
        ),
        _ => return Ok(vec![]),
    };
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    Ok(MigrationRow::find_by_statement(stmt).all(db).await?)
}

pub async fn update_request_payload(
    db: &Db,
    id: Uuid,
    target_class: &str,
    target_type: Option<&str>,
    payload: &Value,
) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(
            request::Column::TargetClass,
            Expr::value(target_class.to_string()),
        )
        .col_expr(
            request::Column::TargetType,
            Expr::value(target_type.map(str::to_string)),
        )
        .col_expr(request::Column::Payload, Expr::value(payload.clone()))
        .filter(request::Column::Id.eq(id))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub async fn list_channels(db: &Db) -> AppResult<Vec<channel::Model>> {
    Ok(channel::Entity::find()
        .order_by_desc(channel::Column::CreatedAt)
        .all(db)
        .await?)
}

pub async fn get_channel(db: &Db, id: Uuid) -> AppResult<Option<channel::Model>> {
    Ok(channel::Entity::find_by_id(id).one(db).await?)
}

pub async fn create_channel(db: &Db, new: &ChannelNew) -> AppResult<channel::Model> {
    let am = channel::ActiveModel {
        sender_class: Set(new.sender_class.clone()),
        sender_kind: Set(new.sender_kind.clone()),
        receiver_class: Set(new.receiver_class.clone()),
        receiver_kind: Set(new.receiver_kind.clone()),
        description: Set(new.description.clone()),
        ..Default::default()
    };
    match am.insert(db).await {
        Ok(m) => Ok(m),
        Err(e) => Err(map_unique_conflict(e, "channel already exists")),
    }
}

pub async fn update_channel(db: &Db, id: Uuid, patch: &ChannelPatch) -> AppResult<()> {
    let mut am = channel::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(AppError::NotFound)?
        .into_active_model();
    if let Some(c) = &patch.sender_class {
        am.sender_class = Set(c.clone());
    }
    if let Some(k) = &patch.sender_kind {
        am.sender_kind = Set(k.clone());
    }
    if let Some(c) = &patch.receiver_class {
        am.receiver_class = Set(c.clone());
    }
    if let Some(k) = &patch.receiver_kind {
        am.receiver_kind = Set(k.clone());
    }
    if let Some(d) = &patch.description {
        am.description = Set(d.clone());
    }
    match am.update(db).await {
        Ok(_) => Ok(()),
        Err(e) => Err(map_unique_conflict(e, "channel already exists")),
    }
}

pub async fn patch_channel(db: &Db, id: Uuid, patch: &ChannelPatch) -> AppResult<channel::Model> {
    update_channel(db, id, patch).await?;
    get_channel(db, id).await?.ok_or(AppError::NotFound)
}

pub async fn delete_channel(db: &Db, id: Uuid) -> AppResult<()> {
    let res = channel::Entity::delete_by_id(id).exec(db).await?;
    if res.rows_affected == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// All channels where (sender_class, sender_kind) allow this agent to send.
/// NULL sender_kind on the channel = any kind of that class.
pub async fn channels_for_sender(
    db: &Db,
    class: &str,
    kind: Option<&str>,
) -> AppResult<Vec<channel::Model>> {
    let kind_sql = match kind {
        Some(k) => format!("'{k}'"),
        None => "NULL".to_string(),
    };
    let sql = format!(
        "SELECT id, sender_class, sender_kind, receiver_class, receiver_kind, description, created_at \
           FROM channels \
          WHERE sender_class = '{class}' \
            AND (sender_kind IS NULL OR sender_kind = {kind_sql}) \
          ORDER BY created_at ASC"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let rows = db.query_all(stmt).await?;
    rows.into_iter()
        .map(|r| channel::Model::from_query_result(&r, ""))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::from)
}

/// Returns Ok(()) iff the channel allows
/// `(sender_class, sender_kind) → (target_class, target_kind)`.
/// Returns Err(NotFound) if the channel doesn't exist; Err(Forbidden) if the
/// sender side doesn't match; Err(BadRequest) if the receiver side doesn't
/// match the target.
pub async fn channel_authorizes(
    db: &Db,
    channel_id: Uuid,
    sender_class: &str,
    sender_kind: Option<&str>,
    target_class: &str,
    target_kind: Option<&str>,
) -> AppResult<()> {
    let ch = get_channel(db, channel_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let sender_ok = ch.sender_class == sender_class
        && (ch.sender_kind.is_none() || ch.sender_kind.as_deref() == sender_kind);
    if !sender_ok {
        return Err(AppError::Forbidden);
    }
    let receiver_ok = ch.receiver_class == target_class
        && (ch.receiver_kind.is_none() || ch.receiver_kind.as_deref() == target_kind);
    if !receiver_ok {
        return Err(AppError::BadRequest(
            "channel does not allow this target".into(),
        ));
    }
    Ok(())
}
