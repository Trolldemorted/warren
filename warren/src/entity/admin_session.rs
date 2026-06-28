use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Index, IndexCreateStatement};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "admin_sessions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub token: String,
    #[sea_orm(default_expr = "Expr::cust(\"now()\")")]
    pub created_at: ChronoDateTimeUtc,
    pub expires_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub fn extra_indexes() -> Vec<IndexCreateStatement> {
    vec![Index::create()
        .name("admin_sessions_expires_idx")
        .table(Entity)
        .col(Column::ExpiresAt)
        .to_owned()]
}
