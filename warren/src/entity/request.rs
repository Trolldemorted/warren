use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{ConditionalStatement, Expr, Index, IndexCreateStatement, IndexOrder};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

pub const PENDING_REQUEST_APPROVAL: i16 = 0;
pub const PENDING_RESPONSE_APPROVAL: i16 = 1;
pub const DONE: i16 = 2;
pub const REJECTED: i16 = 3;

pub fn status_label(s: i16) -> &'static str {
    match s {
        PENDING_REQUEST_APPROVAL => "pending_request_approval",
        PENDING_RESPONSE_APPROVAL => "pending_response_approval",
        DONE => "done",
        REJECTED => "rejected",
        _ => "unknown",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "requests")]
pub struct Model {
    #[sea_orm(
        primary_key,
        auto_increment = false,
        default_expr = "Expr::cust(\"gen_random_uuid()\")"
    )]
    pub id: Uuid,
    pub target_class: String,
    #[sea_orm(column_name = "target_type")]
    #[serde(rename = "target_type")]
    pub target_type: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub payload: Json,
    #[sea_orm(column_type = "JsonBinary")]
    pub response: Option<Json>,
    #[sea_orm(default_expr = "0")]
    pub status: i16,
    pub sender_agent_id: Option<Uuid>,
    pub claimed_by: Option<Uuid>,
    pub claimed_at: Option<ChronoDateTimeUtc>,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
    pub responded_at: Option<ChronoDateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::agent::Entity",
        from = "Column::ClaimedBy",
        to = "super::agent::Column::Id"
    )]
    Agent,
    #[sea_orm(
        belongs_to = "super::agent::Entity",
        from = "Column::SenderAgentId",
        to = "super::agent::Column::Id"
    )]
    Sender,
}

impl Related<super::agent::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Agent.def()
    }
}

impl Model {
    pub fn status_label(&self) -> &'static str {
        status_label(self.status)
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub fn extra_indexes() -> Vec<IndexCreateStatement> {
    vec![
        Index::create()
            .name("requests_inbox_idx")
            .table(Entity)
            .col(Column::TargetClass)
            .col(Column::TargetType)
            .and_where(Expr::col(Column::Status).eq(PENDING_RESPONSE_APPROVAL))
            .and_where(Expr::col(Column::ClaimedBy).is_null())
            .to_owned(),
        Index::create()
            .name("requests_status_idx")
            .table(Entity)
            .col(Column::Status)
            .col((Column::CreatedAt, IndexOrder::Desc))
            .to_owned(),
        Index::create()
            .name("requests_sender_idx")
            .table(Entity)
            .col(Column::SenderAgentId)
            .col((Column::CreatedAt, IndexOrder::Desc))
            .to_owned(),
    ]
}
