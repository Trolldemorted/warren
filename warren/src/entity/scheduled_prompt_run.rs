use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement, IndexOrder};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "scheduled_prompt_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub scheduled_prompt_id: Uuid,
    pub agent_id: Option<Uuid>,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub fired_at: ChronoDateTimeUtc,
    pub finished_at: Option<ChronoDateTimeUtc>,
    pub outcome: String,
    pub skip_reason: Option<String>,
    pub prompt_id: Option<Uuid>,
    pub outcome_error: Option<String>,
    /// Whole-percent snapshot of `weekly_pct` at fire time (None if the
    /// scrape was missing or incomplete).
    pub usage_weekly_pct: Option<i32>,
    pub usage_session_pct: Option<i32>,
    /// Whole-percent snapshot of the `/context` modal's `ctx_used_pct` at
    /// fire time (None if the scrape was missing, incomplete, or no
    /// `context_check` ran). Mirrors `usage_weekly_pct` — same nullable
    /// shape so a missing scrape is indistinguishable from a clean 0%.
    pub usage_context_pct: Option<i32>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::scheduled_prompt::Entity",
        from = "Column::ScheduledPromptId",
        to = "super::scheduled_prompt::Column::Id"
    )]
    ScheduledPrompt,
    #[sea_orm(
        belongs_to = "super::agent::Entity",
        from = "Column::AgentId",
        to = "super::agent::Column::Id"
    )]
    Agent,
}

impl Related<super::scheduled_prompt::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ScheduledPrompt.def()
    }
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
            .name("scheduled_prompt_runs_prompt_idx")
            .table(Entity)
            .col(Column::ScheduledPromptId)
            .col((Column::FiredAt, IndexOrder::Desc))
            .to_owned(),
        Index::create()
            .name("scheduled_prompt_runs_agent_idx")
            .table(Entity)
            .col(Column::AgentId)
            .col((Column::FiredAt, IndexOrder::Desc))
            .to_owned(),
    ]
}
