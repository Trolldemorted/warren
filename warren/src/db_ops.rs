use crate::db::Db;
use crate::entity::{agent, request};
use crate::error::{map_unique_conflict, AppError, AppResult};
use crate::models::{AgentNew, AgentPatch, RequestNew};
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
/// - sent by them (regardless of status)
/// - claimable now (matching class+kind, pending, unclaimed)
/// - claimed by them (any status where they hold the claim)
/// - responded by them (response set, may still be waiting for admin approval)
pub async fn list_requests_for_agent(
    db: &Db,
    agent_id: Uuid,
    class: &str,
    kind: Option<&str>,
) -> AppResult<Vec<request::Model>> {
    let target_type_sql = match kind {
        Some(k) => format!("'{k}'"),
        None => "NULL".to_string(),
    };
    let pending = request::PENDING_RESPONSE_APPROVAL;
    let sql = format!(
        "SELECT id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, created_at, responded_at \
           FROM requests \
          WHERE sender_agent_id = '{agent_id}' \
             OR (status = {pending} \
                 AND claimed_by IS NULL \
                 AND target_class = '{class}' \
                 AND (target_type IS NULL OR target_type = {target_type_sql})) \
             OR claimed_by = '{agent_id}' \
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

pub async fn claim_request(db: &Db, id: Uuid, agent_id: Uuid) -> AppResult<request::Model> {
    let pending = request::PENDING_RESPONSE_APPROVAL;
    let sql = format!(
        "UPDATE requests SET claimed_by = '{agent_id}', claimed_at = NOW() WHERE id = '{id}' AND status = {pending} AND claimed_by IS NULL AND target_class = (SELECT class FROM agents WHERE id = '{agent_id}') AND (target_type IS NULL OR target_type = (SELECT kind FROM agents WHERE id = '{agent_id}')) RETURNING id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, created_at, responded_at"
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
    let pending = request::PENDING_RESPONSE_APPROVAL;
    let sql = format!(
        "UPDATE requests SET response = '{response_json}'::jsonb, responded_at = NOW() WHERE id = '{id}' AND claimed_by = '{agent_id}' AND status = {pending} RETURNING id, target_class, target_type, payload, response, status, sender_agent_id, claimed_by, claimed_at, created_at, responded_at"
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
        .filter(request::Column::Status.eq(request::PENDING_RESPONSE_APPROVAL))
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
        .filter(request::Column::Status.eq(request::PENDING_RESPONSE_APPROVAL))
        .filter(request::Column::Response.is_not_null())
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict("no response to reject".into()));
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
