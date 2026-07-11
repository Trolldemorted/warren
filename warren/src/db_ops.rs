use crate::db::Db;
use crate::entity::{agent, agent_event, channel, request, scheduled_prompt, scheduled_prompt_run};
use crate::error::{map_unique_conflict, AppError, AppResult};
use crate::models::{
    AgentNew, AgentPatch, ChannelNew, ChannelPatch, RequestNew, ScheduledPromptNew,
    ScheduledPromptPatch,
};
use sea_orm::sea_query::{Expr, IntoCondition, Order, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseBackend, EntityTrait, FromQueryResult,
    IntoActiveModel, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
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

pub async fn channel_requires_request_approval(db: &Db, channel_id: Uuid) -> AppResult<bool> {
    Ok(get_channel(db, channel_id)
        .await?
        .ok_or(AppError::NotFound)?
        .requires_request_approval)
}

pub async fn channel_requires_response_approval(db: &Db, channel_id: Uuid) -> AppResult<bool> {
    Ok(get_channel(db, channel_id)
        .await?
        .ok_or(AppError::NotFound)?
        .requires_response_approval)
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

/// Requests actionable for the agent RIGHT NOW:
/// - status 1 (awaiting_agent_request_claim) in their class+kind inbox, unclaimed
/// - status 2 (awaiting_agent_response) where they hold the claim
/// - status 4 (awaiting_agent_response_acknowledge) where they sent the request
pub async fn list_inbox_for_agent(
    db: &Db,
    agent_id: Uuid,
    class: &str,
    kind: Option<&str>,
) -> AppResult<Vec<request::Model>> {
    let inbox_branch = Condition::all()
        .add(request::Column::Status.eq(request::AWAITING_AGENT_REQUEST_CLAIM))
        .add(request::Column::ClaimedBy.is_null())
        .add(request::Column::TargetClass.eq(class.to_string()))
        .add(target_type_match(kind));
    let claimed_branch = Condition::all()
        .add(request::Column::Status.eq(request::AWAITING_AGENT_RESPONSE))
        .add(request::Column::ClaimedBy.eq(agent_id));
    let ack_branch = Condition::all()
        .add(request::Column::Status.eq(request::AWAITING_AGENT_RESPONSE_ACKNOWLEDGE))
        .add(request::Column::SenderAgentId.eq(agent_id));
    let where_clause = Condition::any()
        .add(inbox_branch)
        .add(claimed_branch)
        .add(ack_branch);
    Ok(request::Entity::find()
        .filter(where_clause)
        .order_by_asc(request::Column::CreatedAt)
        .all(db)
        .await?)
}

/// Full history for an agent: every row they sent or received, all statuses.
pub async fn list_history_for_agent(
    db: &Db,
    agent_id: Uuid,
    class: &str,
    kind: Option<&str>,
) -> AppResult<Vec<request::Model>> {
    let sent = Condition::all().add(request::Column::SenderAgentId.eq(agent_id));
    let claimed = Condition::all().add(request::Column::ClaimedBy.eq(agent_id));
    let inbox = Condition::all()
        .add(request::Column::Status.eq(request::AWAITING_AGENT_REQUEST_CLAIM))
        .add(request::Column::ClaimedBy.is_null())
        .add(request::Column::TargetClass.eq(class.to_string()))
        .add(target_type_match(kind));
    let where_clause = Condition::any().add(sent).add(claimed).add(inbox);
    Ok(request::Entity::find()
        .filter(where_clause)
        .order_by_asc(request::Column::CreatedAt)
        .all(db)
        .await?)
}

fn target_type_match(kind: Option<&str>) -> sea_orm::Condition {
    match kind {
        Some(k) => request::Column::TargetType
            .is_null()
            .or(request::Column::TargetType.eq(k.to_string()))
            .into_condition(),
        None => request::Column::TargetType.is_null().into_condition(),
    }
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
    let class_subquery = Query::select()
        .column(agent::Column::Class)
        .from(agent::Entity)
        .and_where(agent::Column::Id.eq(agent_id))
        .to_owned();
    let kind_subquery = Query::select()
        .column(agent::Column::Kind)
        .from(agent::Entity)
        .and_where(agent::Column::Id.eq(agent_id))
        .to_owned();
    let target_type_match = request::Column::TargetType
        .is_null()
        .or(request::Column::TargetType.in_subquery(kind_subquery))
        .into_condition();
    let rows = request::Entity::update_many()
        .col_expr(request::Column::ClaimedBy, Expr::value(Some(agent_id)))
        .col_expr(
            request::Column::ClaimedAt,
            Expr::value(Some(chrono::Utc::now())),
        )
        .col_expr(
            request::Column::Status,
            Expr::value(request::AWAITING_AGENT_RESPONSE),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::AWAITING_AGENT_REQUEST_CLAIM))
        .filter(request::Column::ClaimedBy.is_null())
        .filter(request::Column::TargetClass.in_subquery(class_subquery))
        .filter(target_type_match)
        .exec_with_returning(db)
        .await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| AppError::Conflict("request not in inbox or already claimed".into()))
}

pub async fn respond_to_request(
    db: &Db,
    id: Uuid,
    agent_id: Uuid,
    response: &str,
) -> AppResult<request::Model> {
    let req = get_request(db, id).await?.ok_or(AppError::NotFound)?;
    let requires_approval = match req.channel_id {
        Some(channel_id) => channel_requires_response_approval(db, channel_id).await?,
        None => true,
    };
    let next_status = if requires_approval {
        request::AWAITING_ADMIN_RESPONSE_APPROVAL
    } else {
        request::AWAITING_AGENT_RESPONSE_ACKNOWLEDGE
    };
    let rows = request::Entity::update_many()
        .col_expr(request::Column::Response, Expr::value(response.to_string()))
        .col_expr(
            request::Column::RespondedAt,
            Expr::value(chrono::Utc::now()),
        )
        .col_expr(request::Column::Status, Expr::value(next_status))
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::ClaimedBy.eq(agent_id))
        .filter(request::Column::Status.eq(request::AWAITING_AGENT_RESPONSE))
        .filter(request::Column::Response.is_null())
        .exec_with_returning(db)
        .await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| AppError::Conflict("already responded or not claimed by this agent".into()))
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

/// Admin override: set a request's status to any valid value without
/// checking the current state. Returns the updated row, or NotFound if
/// no row exists with this id.
pub async fn set_request_status_admin(
    db: &Db,
    id: Uuid,
    new_status: i16,
) -> AppResult<request::Model> {
    let rows = request::Entity::update_many()
        .col_expr(request::Column::Status, Expr::value(new_status))
        .filter(request::Column::Id.eq(id))
        .exec_with_returning(db)
        .await?;
    rows.into_iter().next().ok_or(AppError::NotFound)
}

pub async fn accept_request_response(db: &Db, id: Uuid) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(
            request::Column::Status,
            Expr::value(request::AWAITING_AGENT_RESPONSE_ACKNOWLEDGE),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::AWAITING_ADMIN_RESPONSE_APPROVAL))
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
        .col_expr(request::Column::Response, Expr::value(None::<String>))
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
        .col_expr(
            request::Column::Status,
            Expr::value(request::AWAITING_AGENT_REQUEST_CLAIM),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::AWAITING_ADMIN_RESPONSE_APPROVAL))
        .filter(request::Column::Response.is_not_null())
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict("no response to reject".into()));
    }
    Ok(())
}

/// Mark an `awaiting_agent_response_acknowledge` request as `done` by an
/// agent (or by an admin on the agent's behalf). Atomic; if the row is not in
/// that state, or `by_admin = false` and the caller is neither sender nor
/// claimer, returns `Conflict`.
pub async fn acknowledge_request(
    db: &Db,
    id: Uuid,
    caller_id: Uuid,
    by_admin: bool,
) -> AppResult<request::Model> {
    let mut q = request::Entity::update_many()
        .col_expr(request::Column::Status, Expr::value(request::DONE))
        .col_expr(
            request::Column::AcknowledgedAt,
            Expr::value(chrono::Utc::now()),
        )
        .col_expr(
            request::Column::AcknowledgedBy,
            Expr::value(if by_admin { None } else { Some(caller_id) } as Option<Uuid>),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::AWAITING_AGENT_RESPONSE_ACKNOWLEDGE));
    if !by_admin {
        let owned = request::Column::SenderAgentId
            .eq(caller_id)
            .into_condition();
        q = q.filter(owned);
    }
    let rows = q.exec_with_returning(db).await?;
    rows.into_iter()
        .next()
        .ok_or_else(|| AppError::Conflict("request not done yet or not owned by caller".into()))
}

/// Admin-only: revert a `done` request back to awaiting-agent-acknowledge.
pub async fn unacknowledge_request(db: &Db, id: Uuid) -> AppResult<()> {
    let res = request::Entity::update_many()
        .col_expr(
            request::Column::Status,
            Expr::value(request::AWAITING_AGENT_RESPONSE_ACKNOWLEDGE),
        )
        .col_expr(
            request::Column::AcknowledgedAt,
            Expr::value(None as Option<chrono::DateTime<chrono::Utc>>),
        )
        .col_expr(
            request::Column::AcknowledgedBy,
            Expr::value(None as Option<Uuid>),
        )
        .filter(request::Column::Id.eq(id))
        .filter(request::Column::Status.eq(request::DONE))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Err(AppError::Conflict("request not done".into()));
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
    let rows: Vec<AgentClassRow> = agent::Entity::find()
        .select_only()
        .column(agent::Column::Class)
        .distinct()
        .order_by(agent::Column::Class, Order::Asc)
        .into_model()
        .all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.class).collect())
}

pub async fn distinct_agent_kinds(db: &Db) -> AppResult<Vec<String>> {
    let rows: Vec<AgentKindRow> = agent::Entity::find()
        .select_only()
        .column(agent::Column::Kind)
        .distinct()
        .filter(agent::Column::Kind.is_not_null())
        .order_by(agent::Column::Kind, Order::Asc)
        .into_model()
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

pub async fn update_request(
    db: &Db,
    id: Uuid,
    target_class: &str,
    target_type: Option<&str>,
    payload: &str,
    response: Option<&str>,
    status: i16,
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
        .col_expr(request::Column::Payload, Expr::value(payload.to_string()))
        .col_expr(
            request::Column::Response,
            Expr::value(response.map(str::to_string)),
        )
        .col_expr(request::Column::Status, Expr::value(status))
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
        requires_request_approval: Set(new.requires_request_approval),
        requires_response_approval: Set(new.requires_response_approval),
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
    if let Some(b) = patch.requires_request_approval {
        am.requires_request_approval = Set(b);
    }
    if let Some(b) = patch.requires_response_approval {
        am.requires_response_approval = Set(b);
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
    let mut q = channel::Entity::find()
        .filter(channel::Column::SenderClass.eq(class.to_string()))
        .order_by_asc(channel::Column::CreatedAt);
    q = match kind {
        Some(k) => q.filter(
            channel::Column::SenderKind
                .is_null()
                .or(channel::Column::SenderKind.eq(k)),
        ),
        None => q.filter(channel::Column::SenderKind.is_null()),
    };
    Ok(q.all(db).await?)
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

#[allow(dead_code)]
pub async fn insert_agent_event(
    db: &Db,
    id: Uuid,
    agent_id: Uuid,
    seq: i64,
    kind: &str,
    payload: serde_json::Value,
) -> AppResult<agent_event::Model> {
    let am = agent_event::ActiveModel {
        id: Set(id),
        agent_id: Set(agent_id),
        seq: Set(seq),
        kind: Set(kind.to_string()),
        payload: Set(payload),
        ..Default::default()
    };
    Ok(am.insert(db).await?)
}

#[allow(dead_code)]
pub async fn list_events_since(
    db: &Db,
    agent_id: Uuid,
    since_seq: i64,
    limit: u64,
) -> AppResult<Vec<agent_event::Model>> {
    Ok(agent_event::Entity::find()
        .filter(agent_event::Column::AgentId.eq(agent_id))
        .filter(agent_event::Column::Seq.gt(since_seq))
        .order_by_asc(agent_event::Column::Seq)
        .limit(Some(limit))
        .all(db)
        .await?)
}

#[allow(dead_code)]
pub async fn next_event_seq(db: &Db, agent_id: Uuid) -> AppResult<i64> {
    use sea_orm::QuerySelect;
    let row: Option<agent_event::Model> = agent_event::Entity::find()
        .filter(agent_event::Column::AgentId.eq(agent_id))
        .order_by_desc(agent_event::Column::Seq)
        .limit(Some(1))
        .one(db)
        .await?;
    Ok(row.map(|r| r.seq).unwrap_or(0) + 1)
}

// --- scheduled_prompts -------------------------------------------------------

pub async fn list_scheduled_prompts(db: &Db) -> AppResult<Vec<scheduled_prompt::Model>> {
    Ok(scheduled_prompt::Entity::find()
        .order_by_asc(scheduled_prompt::Column::Name)
        .all(db)
        .await?)
}

pub async fn get_scheduled_prompt(db: &Db, id: Uuid) -> AppResult<Option<scheduled_prompt::Model>> {
    Ok(scheduled_prompt::Entity::find_by_id(id).one(db).await?)
}

pub async fn create_scheduled_prompt(
    db: &Db,
    new: &ScheduledPromptNew,
) -> AppResult<scheduled_prompt::Model> {
    let am = scheduled_prompt::ActiveModel {
        id: Set(Uuid::new_v4()),
        name: Set(new.name.clone()),
        prompt_text: Set(new.prompt_text.clone()),
        interval_seconds: Set(new.interval_seconds),
        enabled: Set(new.enabled),
        ignore_inbox_state: Set(new.ignore_inbox_state),
        weekly_safety_buffer_pct: Set(new.weekly_safety_buffer_pct),
        session_safety_buffer_pct: Set(new.session_safety_buffer_pct),
        target_class: Set(new.target_class.trim().to_string()),
        target_kind: Set(new
            .target_kind
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())),
        next_fire_at: Set(Some(chrono::Utc::now())),
        ..Default::default()
    };
    Ok(am.insert(db).await?)
}

pub async fn update_scheduled_prompt(
    db: &Db,
    id: Uuid,
    patch: &ScheduledPromptPatch,
) -> AppResult<scheduled_prompt::Model> {
    let mut am = scheduled_prompt::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(AppError::NotFound)?
        .into_active_model();
    if let Some(n) = &patch.name {
        am.name = Set(n.clone());
    }
    if let Some(t) = &patch.prompt_text {
        am.prompt_text = Set(t.clone());
    }
    if let Some(i) = patch.interval_seconds {
        am.interval_seconds = Set(i);
    }
    if let Some(e) = patch.enabled {
        am.enabled = Set(e);
    }
    if let Some(b) = patch.ignore_inbox_state {
        am.ignore_inbox_state = Set(b);
    }
    if let Some(w) = patch.weekly_safety_buffer_pct {
        am.weekly_safety_buffer_pct = Set(w);
    }
    if let Some(s) = patch.session_safety_buffer_pct {
        am.session_safety_buffer_pct = Set(s);
    }
    am.updated_at = Set(chrono::Utc::now());
    am.update(db).await?;
    get_scheduled_prompt(db, id)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn delete_scheduled_prompt(db: &Db, id: Uuid) -> AppResult<()> {
    // The runs are reachable only via their parent prompt (every
    // consumer filters by `scheduled_prompt_id`), so once the prompt is
    // gone the runs have no UI/API surface to read them. Cascade at
    // the application layer so the FK doesn't block the delete.
    let txn = db.begin().await?;
    scheduled_prompt_run::Entity::delete_many()
        .filter(scheduled_prompt_run::Column::ScheduledPromptId.eq(id))
        .exec(&txn)
        .await?;
    let res = scheduled_prompt::Entity::delete_by_id(id).exec(&txn).await?;
    txn.commit().await?;
    if res.rows_affected == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Atomically claim due scheduled prompts: select rows where
/// `enabled = true AND next_fire_at <= now()`, set `next_fire_at = NULL`
/// to prevent double-fires across overlapping ticks or restarts, and
/// return the claimed rows. Uses a single transaction so a concurrent
/// scheduler tick sees an empty set.
pub async fn claim_due_scheduled_prompts(
    db: &Db,
    now: chrono::DateTime<chrono::Utc>,
    limit: u64,
) -> AppResult<Vec<scheduled_prompt::Model>> {
    let due = scheduled_prompt::Entity::find()
        .filter(scheduled_prompt::Column::Enabled.eq(true))
        .filter(scheduled_prompt::Column::NextFireAt.lte(now))
        .order_by_asc(scheduled_prompt::Column::NextFireAt)
        .limit(Some(limit))
        .all(db)
        .await?;
    if due.is_empty() {
        return Ok(due);
    }
    let ids: Vec<Uuid> = due.iter().map(|m| m.id).collect();
    scheduled_prompt::Entity::update_many()
        .col_expr(
            scheduled_prompt::Column::NextFireAt,
            Expr::value(None::<chrono::DateTime<chrono::Utc>>),
        )
        .filter(scheduled_prompt::Column::Id.is_in(ids))
        .exec(db)
        .await?;
    Ok(due)
}

/// Update `next_fire_at` and `last_fired_at` after a successful
/// submission. The new value is computed by the caller (typically
/// `last_finished_at + interval_seconds` once observation completes,
/// or `now + interval_seconds` on skip).
pub async fn set_next_fire_at(
    db: &Db,
    id: Uuid,
    next_fire_at: chrono::DateTime<chrono::Utc>,
    last_fired_at: chrono::DateTime<chrono::Utc>,
) -> AppResult<()> {
    let mut am = scheduled_prompt::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(AppError::NotFound)?
        .into_active_model();
    am.next_fire_at = Set(Some(next_fire_at));
    am.last_fired_at = Set(Some(last_fired_at));
    am.updated_at = Set(chrono::Utc::now());
    am.update(db).await?;
    Ok(())
}

/// Mark `last_finished_at` once observation completes. The scheduler
/// uses this to advance the interval anchor for the next slot.
pub async fn mark_scheduled_prompt_finished(
    db: &Db,
    id: Uuid,
    finished_at: chrono::DateTime<chrono::Utc>,
) -> AppResult<()> {
    let mut am = scheduled_prompt::Entity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(AppError::NotFound)?
        .into_active_model();
    am.last_finished_at = Set(Some(finished_at));
    am.updated_at = Set(chrono::Utc::now());
    am.update(db).await?;
    Ok(())
}

// --- scheduled_prompt_runs ---------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn insert_run_started(
    db: &Db,
    scheduled_prompt_id: Uuid,
    agent_id: Option<Uuid>,
    outcome: &str,
    prompt_id: Option<Uuid>,
    usage_weekly_pct: Option<i32>,
    usage_session_pct: Option<i32>,
    skip_reason: Option<&str>,
) -> AppResult<scheduled_prompt_run::Model> {
    let am = scheduled_prompt_run::ActiveModel {
        id: Set(Uuid::new_v4()),
        scheduled_prompt_id: Set(scheduled_prompt_id),
        agent_id: Set(agent_id),
        fired_at: Set(chrono::Utc::now()),
        finished_at: Set(None),
        outcome: Set(outcome.to_string()),
        skip_reason: Set(skip_reason.map(str::to_string)),
        prompt_id: Set(prompt_id),
        outcome_error: Set(None),
        usage_weekly_pct: Set(usage_weekly_pct),
        usage_session_pct: Set(usage_session_pct),
    };
    Ok(am.insert(db).await?)
}

/// Finalize a run: set `finished_at`, override `outcome` (e.g.
/// `'completed'`, `'needs_input_canceled'`, `'warren_restart'`), and
/// optionally store an error message.
pub async fn finalize_run(
    db: &Db,
    run_id: Uuid,
    outcome: &str,
    outcome_error: Option<&str>,
) -> AppResult<()> {
    let mut am = scheduled_prompt_run::Entity::find_by_id(run_id)
        .one(db)
        .await?
        .ok_or(AppError::NotFound)?
        .into_active_model();
    am.outcome = Set(outcome.to_string());
    am.outcome_error = Set(outcome_error.map(str::to_string));
    am.finished_at = Set(Some(chrono::Utc::now()));
    am.update(db).await?;
    Ok(())
}

pub async fn list_runs_for_scheduled_prompt(
    db: &Db,
    scheduled_prompt_id: Uuid,
    limit: u64,
) -> AppResult<Vec<scheduled_prompt_run::Model>> {
    Ok(scheduled_prompt_run::Entity::find()
        .filter(scheduled_prompt_run::Column::ScheduledPromptId.eq(scheduled_prompt_id))
        .order_by_desc(scheduled_prompt_run::Column::FiredAt)
        .limit(Some(limit))
        .all(db)
        .await?)
}

/// Find runs that were fired but never finalized — used by the
/// observation sweep to detect lost prompts.
pub async fn list_unfinalized_runs(
    db: &Db,
    older_than: chrono::DateTime<chrono::Utc>,
    limit: u64,
) -> AppResult<Vec<scheduled_prompt_run::Model>> {
    Ok(scheduled_prompt_run::Entity::find()
        .filter(scheduled_prompt_run::Column::Outcome.eq("fired"))
        .filter(scheduled_prompt_run::Column::FinishedAt.is_null())
        .filter(scheduled_prompt_run::Column::FiredAt.lt(older_than))
        .order_by_asc(scheduled_prompt_run::Column::FiredAt)
        .limit(Some(limit))
        .all(db)
        .await?)
}

/// Cross-restart reconciliation: every run row in `'fired'` state
/// without a `finished_at` is presumed lost (warren died mid-prompt,
/// rabbit dropped, etc.). Flip them to `'warren_restart'`, set
/// `finished_at = now`, and recompute `next_fire_at` for the parent
/// schedule so the tick loop picks it up on the next pass. Idempotent:
/// the `finished_at IS NULL` filter skips already-reconciled rows.
pub async fn reconcile_after_restart(db: &Db) -> AppResult<u64> {
    let now = chrono::Utc::now();
    let stale = list_unfinalized_runs(db, now - chrono::Duration::seconds(5), 1000).await?;
    let mut reconciled: u64 = 0;
    for run in stale {
        finalize_run(db, run.id, "warren_restart", Some("warren_restart")).await?;
        // Advance the parent schedule by one interval from now.
        if let Some(p) = get_scheduled_prompt(db, run.scheduled_prompt_id).await? {
            let next = now + chrono::Duration::seconds(p.interval_seconds);
            set_next_fire_at(db, p.id, next, now).await?;
            mark_scheduled_prompt_finished(db, p.id, now).await?;
        }
        reconciled += 1;
    }
    Ok(reconciled)
}

#[derive(FromQueryResult)]
struct CountRow {
    pub count: i64,
}

/// Find every agent whose `(class, kind)` matches the supplied target.
/// `kind = None` matches agents whose own `kind IS NULL`; `kind =
/// Some(k)` matches agents where `kind = k OR kind IS NULL` (the same
/// inbox-match semantic used by `target_type_match`).
/// Returns rows ordered by `created_at ASC` so a deterministic caller
/// (e.g. the scheduler) gets a stable choice.
pub async fn list_agents_by_class_kind(
    db: &Db,
    class: &str,
    kind: Option<&str>,
) -> AppResult<Vec<agent::Model>> {
    let mut q = agent::Entity::find().filter(agent::Column::Class.eq(class.to_string()));
    q = q.filter(match kind {
        Some(k) => agent::Column::Kind
            .is_null()
            .or(agent::Column::Kind.eq(k.to_string()))
            .into_condition(),
        None => agent::Column::Kind.is_null().into_condition(),
    });
    Ok(q.order_by_asc(agent::Column::CreatedAt).all(db).await?)
}

/// Count inbox rows actionable for any agent with the given
/// `(target_class, target_kind)` right now. Mirrors branch A of
/// `count_inbox_for_agent` but parameterized directly on the address
/// rather than a specific agent. Used by the scheduler to decide
/// whether to fire when `ignore_inbox_state = false`.
pub async fn count_inbox_by_target(db: &Db, class: &str, kind: Option<&str>) -> AppResult<u64> {
    use sea_orm::QuerySelect;
    let kind_cond = match kind {
        Some(k) => request::Column::TargetType
            .is_null()
            .or(request::Column::TargetType.eq(k.to_string()))
            .into_condition(),
        None => request::Column::TargetType.is_null().into_condition(),
    };
    let row: Option<i64> = request::Entity::find()
        .select_only()
        .column_as(request::Column::Id.count(), "count")
        .filter(request::Column::Status.eq(1_i16))
        .filter(request::Column::ClaimedBy.is_null())
        .filter(request::Column::TargetClass.eq(class.to_string()))
        .filter(kind_cond)
        .into_model::<CountRow>()
        .one(db)
        .await?
        .map(|r| r.count);
    Ok(row.unwrap_or(0).max(0) as u64)
}
