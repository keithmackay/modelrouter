#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::repositories::app_settings::AppSettingsRepository;
use super::{now_utc, PostgresDb};

#[async_trait]
impl AppSettingsRepository for PostgresDb {
    async fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM app_settings WHERE key = $1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(v,)| v))
    }

    async fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO app_settings (key, value, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_utc())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
