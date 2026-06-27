use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "memos")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: Uuid,
    pub target_class: String,
    #[sea_orm(column_name = "target_type")]
    #[serde(rename = "target_type")]
    pub target_type: Option<String>,
    pub payload: Json,
    pub status: String,
    pub created_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
