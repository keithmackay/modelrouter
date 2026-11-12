use async_trait::async_trait;

use super::SqliteDb;
use crate::db::repositories::health::HealthRepository;

#[async_trait]
impl HealthRepository for SqliteDb {
    async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}
