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
    pub kind: Option<String>,
    pub model: String,
    #[sea_orm(default_value = "")]
    pub prompt: String,
    #[sea_orm(unique)]
    pub authtoken: String,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
    /// Forgejo labels whose open *unassigned* issues/PRs this agent's
    /// team can claim — surfaced on the agents-page "Claimable"
    /// column and consulted by the scheduler pre-fire gate as a
    /// fallback when the firing schedule has no `additional_labels` of
    /// its own. OR semantics — an unassigned item counts if it carries
    /// any one of these labels. Empty (the default) preserves today's
    /// "fall back to `[class]`" behavior so existing rows don't change.
    /// The `Vec<String>` field type materializes as `text[]` via the
    /// blanket `ValueType` impl in `sea-query` — mirrors
    /// `scheduled_prompts.additional_labels`.
    #[sea_orm(default_expr = "Expr::cust(\"'{}'::text[]\")")]
    pub claimable_labels: Vec<String>,
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
        .name("agents_class_kind_idx")
        .table(Entity)
        .col(Column::Class)
        .col(Column::Kind)
        .to_owned()]
}
