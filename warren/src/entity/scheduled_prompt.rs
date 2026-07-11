use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "scheduled_prompts")]
pub struct Model {
    #[sea_orm(
        primary_key,
        auto_increment = false,
        default_expr = "Expr::cust(\"gen_random_uuid()\")"
    )]
    pub id: Uuid,
    pub agent_id: Uuid,
    pub name: String,
    pub prompt_text: String,
    pub interval_seconds: i64,
    pub enabled: bool,
    pub ignore_inbox_state: bool,
    /// Whole-percent headroom to keep clear of the limit. Validation
    /// enforces 0..=100 in the API/UI; stored as `i32` so the
    /// `DeriveEntityModel` `Eq` bound is satisfied (floats are not
    /// `Eq`).
    pub weekly_safety_buffer_pct: i32,
    pub session_safety_buffer_pct: i32,
    pub last_fired_at: Option<ChronoDateTimeUtc>,
    pub last_finished_at: Option<ChronoDateTimeUtc>,
    pub next_fire_at: Option<ChronoDateTimeUtc>,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub updated_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::agent::Entity",
        from = "Column::AgentId",
        to = "super::agent::Column::Id"
    )]
    Agent,
    #[sea_orm(has_many = "super::scheduled_prompt_run::Entity")]
    Runs,
}

impl Related<super::agent::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Agent.def()
    }
}

impl Related<super::scheduled_prompt_run::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Runs.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub fn extra_indexes() -> Vec<IndexCreateStatement> {
    vec![
        Index::create()
            .name("scheduled_prompts_next_fire_idx")
            .table(Entity)
            .col(Column::NextFireAt)
            .to_owned(),
        Index::create()
            .name("scheduled_prompts_agent_idx")
            .table(Entity)
            .col(Column::AgentId)
            .to_owned(),
    ]
}
