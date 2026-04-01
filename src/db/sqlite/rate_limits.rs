use async_trait::async_trait;
use super::SqliteDb;
use crate::db::repositories::rate_limits::RateLimitRepository;

#[async_trait]
impl RateLimitRepository for SqliteDb {
    async fn get_request_count(&self, user_id: i64, window_key: &str) -> anyhow::Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT request_count FROM rate_limit_state WHERE user_id = ? AND window_key = ?",
        )
        .bind(user_id)
        .bind(window_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(c,)| c).unwrap_or(0))
    }

    async fn increment_request_count(&self, user_id: i64, window_key: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO rate_limit_state (user_id, window_key, request_count) VALUES (?, ?, 1)
             ON CONFLICT(user_id, window_key) DO UPDATE SET request_count = request_count + 1",
        )
        .bind(user_id)
        .bind(window_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
