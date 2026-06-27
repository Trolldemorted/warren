use sea_orm::{Database, DatabaseConnection};

pub type Db = DatabaseConnection;

pub async fn connect(database_url: &str) -> anyhow::Result<Db> {
    Ok(Database::connect(database_url).await?)
}
