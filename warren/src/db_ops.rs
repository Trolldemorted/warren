use crate::db::Db;
use crate::entity::{agent, memo, request};
use crate::error::{map_unique_conflict, AppError, AppResult};
use crate::models::{AgentNew, AgentPatch, MemoNew, RequestNew};
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
    let status = if new.approved { "approved" } else { "pending" };
    let am = request::ActiveModel {
        target_class: Set(new.target_class.clone()),
        target_type: Set(new.target_type.clone()),
        payload: Set(new.payload.clone()),
        status: Set(status.into()),
        ..Default::default()
    };
    Ok(am.insert(db).await?)
}

pub async fn list_all_requests(
    db: &Db,
    status_filter: Option<&str>,
    limit: u64,
    offset: u64,
) -> AppResult<Vec<request::Model>> {
    let mut q = request::Entity::find().order_by_desc(request::Column::CreatedAt);
    if let Some(s) = status_filter {
        q = q.filter(request::Column::Status.eq(s.to_string()));
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
    let sql = format!(
        "SELECT id, target_class, target_type, payload, response, status, claimed_by, claimed_at, created_at, responded_at \
         FROM requests \
         WHERE status = 'approved' AND claimed_by IS NULL \
           AND target_class = '{class}' \
           AND (target_type IS NULL OR target_type = {target_type_sql}) \
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
    let sql = format!(
        "UPDATE requests SET claimed_by = '{agent_id}', claimed_at = NOW() \
         WHERE id = '{id}' AND status = 'approved' AND claimed_by IS NULL \
           AND target_class = (SELECT class FROM agents WHERE id = '{agent_id}') \
           AND (target_type IS NULL OR target_type = (SELECT type FROM agents WHERE id = '{agent_id}')) \
         RETURNING id, target_class, target_type, payload, response, status, claimed_by, claimed_at, created_at, responded_at"
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
    let sql = format!(
        "UPDATE requests SET response = '{response_json}'::jsonb, status = 'responded', responded_at = NOW() \
         WHERE id = '{id}' AND claimed_by = '{agent_id}' AND status = 'approved' \
         RETURNING id, target_class, target_type, payload, response, status, claimed_by, claimed_at, created_at, responded_at"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let row = db.query_one(stmt).await?;
    match row {
        Some(r) => Ok(request::Model::from_query_result(&r, "")?),
        None => Err(AppError::Conflict("not claimed by this agent".into())),
    }
}

pub async fn set_request_status(db: &Db, id: Uuid, from: &str, to: &str) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(request::Column::Status, Expr::value(to.to_string()))
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(from.to_string()))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict(format!(
            "request not in status '{from}'"
        )));
    }
    Ok(())
}

pub async fn create_memo(db: &Db, new: &MemoNew) -> AppResult<memo::Model> {
    let status = if new.approved { "approved" } else { "pending" };
    let am = memo::ActiveModel {
        target_class: Set(new.target_class.clone()),
        target_type: Set(new.target_type.clone()),
        payload: Set(new.payload.clone()),
        status: Set(status.into()),
        ..Default::default()
    };
    Ok(am.insert(db).await?)
}

pub async fn list_all_memos(
    db: &Db,
    status_filter: Option<&str>,
    limit: u64,
    offset: u64,
) -> AppResult<Vec<memo::Model>> {
    let mut q = memo::Entity::find().order_by_desc(memo::Column::CreatedAt);
    if let Some(s) = status_filter {
        q = q.filter(memo::Column::Status.eq(s.to_string()));
    }
    Ok(q.limit(Some(limit)).offset(Some(offset)).all(db).await?)
}

pub async fn list_memo_inbox(
    db: &Db,
    class: &str,
    kind: Option<&str>,
    agent_id: Uuid,
) -> AppResult<Vec<memo::Model>> {
    let target_type_sql = match kind {
        Some(k) => format!("'{k}'"),
        None => "NULL".to_string(),
    };
    let sql = format!(
        "SELECT id, target_class, target_type, payload, status, created_at FROM memos \
         WHERE status = 'approved' \
           AND target_class = '{class}' \
           AND (target_type IS NULL OR target_type = {target_type_sql}) \
           AND NOT EXISTS (SELECT 1 FROM memo_acks WHERE memo_id = memos.id AND agent_id = '{agent_id}') \
         ORDER BY created_at ASC"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    let rows = db.query_all(stmt).await?;
    rows.into_iter()
        .map(|r| memo::Model::from_query_result(&r, ""))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::from)
}

pub async fn get_memo(db: &Db, id: Uuid) -> AppResult<Option<memo::Model>> {
    Ok(memo::Entity::find_by_id(id).one(db).await?)
}

pub async fn acknowledge_memo(db: &Db, id: Uuid, agent_id: Uuid) -> AppResult<()> {
    let sql = format!(
        "INSERT INTO memo_acks (memo_id, agent_id) VALUES ('{id}', '{agent_id}') ON CONFLICT DO NOTHING"
    );
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql);
    db.execute(stmt).await?;
    Ok(())
}

pub async fn set_memo_status(db: &Db, id: Uuid, from: &str, to: &str) -> AppResult<()> {
    let res = memo::Entity::update_many()
        .col_expr(memo::Column::Status, Expr::value(to.to_string()))
        .filter(memo::Column::Id.eq(id))
        .filter(memo::Column::Status.eq(from.to_string()))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict(format!("memo not in status '{from}'")));
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
