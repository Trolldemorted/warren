use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agents")]
pub struct Model {
    #[sea_orm(
        primary_key,
        auto_increment = false,
        default_expr = "Expr::cust(\"gen_random_uuid()\")"
    )]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub name: String,
    pub class: String,
    #[sea_orm(column_name = "type")]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub model: String,
    #[sea_orm(default_value = "")]
    pub prompt: String,
    #[sea_orm(unique)]
    pub authtoken: String,
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
        .name("agents_class_type_idx")
        .table(Entity)
        .col(Column::Class)
        .col(Column::Kind)
        .to_owned()]
}
