use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let sql = include_str!("../../migrations/m20260101_000001_init.sql");
        for stmt in split_sql(sql) {
            manager.get_connection().execute_unprepared(stmt).await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

fn split_sql(sql: &str) -> Vec<&str> {
    sql.split(";\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}
