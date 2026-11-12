#![cfg(feature = "postgres")]

use async_trait::async_trait;

use super::PostgresDb;
use crate::db::repositories::health::HealthRepository;

#[async_trait]
impl HealthRepository for PostgresDb {
    async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}
