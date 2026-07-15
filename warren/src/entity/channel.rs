use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "channels")]
pub struct Model {
    #[sea_orm(
        primary_key,
        auto_increment = false,
        default_expr = "Expr::cust(\"gen_random_uuid()\")"
    )]
    pub id: Uuid,
    pub sender_class: String,
    pub sender_kind: Option<String>,
    pub receiver_class: String,
    pub receiver_kind: Option<String>,
    #[sea_orm(default_value = "")]
    pub description: String,
    #[sea_orm(default_value = "true")]
    pub requires_request_approval: bool,
    #[sea_orm(default_value = "true")]
    pub requires_response_approval: bool,
    #[sea_orm(default_value = "true")]
    pub enabled: bool,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::request::Entity")]
    Requests,
}

impl Related<super::request::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Requests.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub fn extra_indexes() -> Vec<IndexCreateStatement> {
    vec![Index::create()
        .name("channels_uniq_idx")
        .table(Entity)
        .col(Column::SenderClass)
        .col(Column::SenderKind)
        .col(Column::ReceiverClass)
        .col(Column::ReceiverKind)
        .unique()
        .to_owned()]
}
