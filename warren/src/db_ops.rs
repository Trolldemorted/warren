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

pub async fn create_request(db: &Db, new: &RequestNew) -> AppResult<request::Model> {
    let am = request::ActiveModel {
        target_class: Set(new.target_class.clone()),
        target_type: Set(new.target_type.clone()),
        payload: Set(new.payload.clone()),
        status: Set(request::PENDING_REQUEST_APPROVAL),
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

pub async fn list_inbox(
    db: &Db,
    class: &str,
    kind: Option<&str>,
) -> AppResult<Vec<request::Model>> {
    let target_type_sql = match kind {
        Some(k) => format!("'{k}'"),
        None => "NULL".to_string(),
    };
    let pending = request::PENDING_RESPONSE_APPROVAL;
    let sql = format!(
        "SELECT id, target_class, target_type, payload, response, status, claimed_by, claimed_at, created_at, responded_at FROM requests WHERE status = {pending} AND claimed_by IS NULL AND target_class = '{class}' AND (target_type IS NULL OR target_type = {target_type_sql}) ORDER BY created_at ASC"
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
        "UPDATE requests SET claimed_by = '{agent_id}', claimed_at = NOW() WHERE id = '{id}' AND status = {pending} AND claimed_by IS NULL AND target_class = (SELECT class FROM agents WHERE id = '{agent_id}') AND (target_type IS NULL OR target_type = (SELECT type FROM agents WHERE id = '{agent_id}')) RETURNING id, target_class, target_type, payload, response, status, claimed_by, claimed_at, created_at, responded_at"
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
        "UPDATE requests SET response = '{response_json}'::jsonb, responded_at = NOW() WHERE id = '{id}' AND claimed_by = '{agent_id}' AND status = {pending} RETURNING id, target_class, target_type, payload, response, status, claimed_by, claimed_at, created_at, responded_at"
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
    #[sea_orm(from_alias = "kind")]
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
        "SELECT DISTINCT \"type\" AS kind FROM agents WHERE \"type\" IS NOT NULL ORDER BY \"type\""
            .to_string(),
    ))
    .all(db)
    .await?;
    Ok(rows.into_iter().filter_map(|r| r.kind).collect())
}

#[derive(FromQueryResult)]
pub struct MigrationRow {
    pub version: String,
    pub description: Option<String>,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn is_missing_table(e: &sea_orm::DbErr) -> bool {
    use sea_orm::RuntimeErr;
    if let sea_orm::DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(db))) = e {
        return db.code().as_deref() == Some("42P01");
    }
    false
}

pub async fn list_migrations(db: &Db) -> AppResult<Vec<MigrationRow>> {
    let candidates = [
        ("atlas_schema_revisions", "applied"),
        ("schema_migrations", "installed_on"),
    ];
    for (table, ts_col) in candidates {
        let sql = format!(
            "SELECT version::text AS version, description, {ts_col} AS applied_at \
             FROM {table} ORDER BY version"
        );
        let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
        match MigrationRow::find_by_statement(stmt).all(db).await {
            Ok(rows) => return Ok(rows),
            Err(e) if is_missing_table(&e) => continue,
            Err(e) => return Err(AppError::Db(e)),
        }
    }
    Ok(vec![])
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
