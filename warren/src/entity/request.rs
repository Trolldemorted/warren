use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "requests")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    pub target_class: String,
    #[sea_orm(column_name = "target_type")]
    #[serde(rename = "target_type")]
    pub target_type: Option<String>,
    pub payload: Json,
    pub response: Option<Json>,
    pub status: String,
    pub claimed_by: Option<Uuid>,
    pub claimed_at: Option<ChronoDateTimeUtc>,
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
}

impl Related<super::agent::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Agent.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
