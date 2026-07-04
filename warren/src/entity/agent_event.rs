use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement, IndexOrder};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub agent_id: Uuid,
    pub seq: i64,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub ts: ChronoDateTimeUtc,
    pub kind: String,
    #[sea_orm(column_type = "Json")]
    pub payload: serde_json::Value,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::agent::Entity",
        from = "Column::AgentId",
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

pub fn extra_indexes() -> Vec<IndexCreateStatement> {
    vec![
        Index::create()
            .name("agent_events_agent_seq_idx")
            .table(Entity)
            .col(Column::AgentId)
            .col(Column::Seq)
            .unique()
            .to_owned(),
        Index::create()
            .name("agent_events_agent_ts_idx")
            .table(Entity)
            .col(Column::AgentId)
            .col((Column::Ts, IndexOrder::Desc))
            .to_owned(),
    ]
}
