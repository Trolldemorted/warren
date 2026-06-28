use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use std::time::Duration;

pub type Db = DatabaseConnection;

pub async fn connect(database_url: &str) -> anyhow::Result<Db> {
    let mut opts = ConnectOptions::new(database_url);
    opts.connect_timeout(Duration::from_secs(5));
    opts.acquire_timeout(Duration::from_secs(5));
    opts.sqlx_logging(false);
    Ok(Database::connect(opts).await?)
}
