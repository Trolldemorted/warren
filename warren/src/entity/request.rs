use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{ConditionalStatement, Expr, Index, IndexCreateStatement, IndexOrder};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as Json;

pub const PENDING_REQUEST_APPROVAL: i16 = 0;
pub const AWAITING_RESPONSE: i16 = 1;
pub const DONE: i16 = 2;
pub const REJECTED: i16 = 3;
pub const ACKNOWLEDGED: i16 = 4;

pub fn status_label(s: i16) -> &'static str {
    match s {
        PENDING_REQUEST_APPROVAL => "pending_request_approval",
        AWAITING_RESPONSE => "awaiting_response",
        DONE => "done",
        REJECTED => "rejected",
        ACKNOWLEDGED => "acknowledged",
        _ => "unknown",
    }
}

fn label_to_status(label: &str) -> Option<i16> {
    match label {
        "pending_request_approval" => Some(PENDING_REQUEST_APPROVAL),
        "awaiting_response" => Some(AWAITING_RESPONSE),
        "done" => Some(DONE),
        "rejected" => Some(REJECTED),
        "acknowledged" => Some(ACKNOWLEDGED),
        _ => None,
    }
}

fn serialize_status<S: Serializer>(s: &i16, ser: S) -> Result<S::Ok, S::Error> {
    ser.serialize_str(status_label(*s))
}

fn deserialize_status<'de, D: Deserializer<'de>>(de: D) -> Result<i16, D::Error> {
    let label = String::deserialize(de)?;
    label_to_status(&label)
        .ok_or_else(|| D::Error::custom(format!("unknown request status '{label}'")))
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
    #[serde(
        serialize_with = "serialize_status",
        deserialize_with = "deserialize_status"
    )]
    pub status: i16,
    pub sender_agent_id: Option<Uuid>,
    pub claimed_by: Option<Uuid>,
    pub claimed_at: Option<ChronoDateTimeUtc>,
    pub channel_id: Option<Uuid>,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
    pub responded_at: Option<ChronoDateTimeUtc>,
    pub acknowledged_at: Option<ChronoDateTimeUtc>,
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
    #[sea_orm(
        belongs_to = "super::channel::Entity",
        from = "Column::ChannelId",
        to = "super::channel::Column::Id"
    )]
    Channel,
}

impl Related<super::agent::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Agent.def()
    }
}

impl Related<super::channel::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Channel.def()
    }
}

impl Model {
    pub fn status_label(&self) -> &'static str {
        status_label(self.status)
    }

    pub fn payload_preview(&self) -> String {
        preview_json(&self.payload, 120)
    }

    pub fn response_preview(&self) -> Option<String> {
        self.response.as_ref().map(|r| preview_json(r, 120))
    }
}

fn preview_json(v: &Json, max_chars: usize) -> String {
    let s = v.to_string();
    let n = s.chars().count();
    if n <= max_chars {
        s
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
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
            .and_where(Expr::col(Column::Status).eq(AWAITING_RESPONSE))
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
