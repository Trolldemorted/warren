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
    pub name: String,
    pub prompt_text: String,
    pub interval_seconds: i64,
    #[sea_orm(default_value = "true")]
    pub enabled: bool,
    #[sea_orm(default_value = "false")]
    pub ignore_inbox_state: bool,
    /// Whole-percent headroom to keep clear of the limit. Validation
    /// enforces 0..=100 in the API/UI; stored as `i32` so the
    /// `DeriveEntityModel` `Eq` bound is satisfied (floats are not
    /// `Eq`).
    #[sea_orm(default_value = "0")]
    pub weekly_safety_buffer_pct: i32,
    #[sea_orm(default_value = "0")]
    pub session_safety_buffer_pct: i32,
    pub last_fired_at: Option<ChronoDateTimeUtc>,
    pub last_finished_at: Option<ChronoDateTimeUtc>,
    pub next_fire_at: Option<ChronoDateTimeUtc>,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub updated_at: ChronoDateTimeUtc,
    /// Worker pool address: at fire time the scheduler picks any
    /// connected idle agent with `class = target_class` and
    /// `kind IS NOT DISTINCT FROM target_kind`. Nullable because
    /// `scope='agent'` rows carry an `agent_id` instead.
    pub target_class: Option<String>,
    pub target_kind: Option<String>,
    /// Absolute-token threshold on the scraped `/context` `ctx_used_tokens`
    /// at fire time. `None` or `Some(0)` disables auto-`/clear`; any
    /// positive value clears before submitting the prompt when
    /// `ctx_used_tokens >= this`. Stored as `i64` so the
    /// `DeriveEntityModel` `Eq` bound is satisfied (floats are not `Eq`)
    /// while still covering the largest model contexts.
    pub context_clear_threshold_tokens: Option<i64>,
    /// `team` keeps the historical `(target_class, target_kind)` pool
    /// semantics — at fire time any connected idle agent matching
    /// the pool picks the prompt up. `agent` targets a specific
    /// `agent_id`; the scheduler's pre-fire gate counts the target
    /// agent's unblocked forgejo items unless `ignore_pending_forgejo_work`.
    /// Existing rows have `scope='team'` after the migration.
    #[sea_orm(default_value = "team")]
    pub scope: String,
    /// Specific agent address; only set when `scope='agent'`. A CHECK
    /// constraint enforces the inverse (target_class NULL) and the
    /// FK is ON DELETE CASCADE so deleting an agent clears its
    /// schedules.
    pub agent_id: Option<Uuid>,
    /// Agent-scope only: when false (the default) the schedule skips
    /// with `skipped_no_forgejo_items` if the target agent has no
    /// unblocked forgejo items at fire time. When true, the schedule
    /// fires regardless of forgejo-item count. Has no effect for
    /// team-scope rows.
    #[sea_orm(default_value = "false")]
    pub ignore_pending_forgejo_work: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::scheduled_prompt_run::Entity")]
    Runs,
    #[sea_orm(
        belongs_to = "super::agent::Entity",
        from = "Column::AgentId",
        to = "super::agent::Column::Id"
    )]
    Agent,
}

impl Related<super::scheduled_prompt_run::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Runs.def()
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
            .name("scheduled_prompts_next_fire_idx")
            .table(Entity)
            .col(Column::NextFireAt)
            .to_owned(),
        Index::create()
            .name("scheduled_prompts_target_idx")
            .table(Entity)
            .col(Column::TargetClass)
            .col(Column::TargetKind)
            .to_owned(),
        Index::create()
            .name("scheduled_prompts_agent_idx")
            .table(Entity)
            .col(Column::AgentId)
            .to_owned(),
    ]
}
