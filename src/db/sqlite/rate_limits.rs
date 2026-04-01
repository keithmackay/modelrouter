use async_trait::async_trait;
use super::SqliteDb;
use crate::db::repositories::rate_limits::RateLimitRepository;

#[async_trait]
impl RateLimitRepository for SqliteDb {
    async fn increment_and_get_count(&self, user_id: i64, window_key: &str) -> anyhow::Result<i64> {
        sqlx::query(
            "INSERT INTO rate_limit_state (user_id, window_key, request_count) VALUES (?, ?, 1)
             ON CONFLICT(user_id, window_key) DO UPDATE SET request_count = request_count + 1",
        )
        .bind(user_id)
        .bind(window_key)
        .execute(&self.pool)
        .await?;

        let count: (i64,) = sqlx::query_as(
            "SELECT request_count FROM rate_limit_state WHERE user_id = ? AND window_key = ?",
        )
        .bind(user_id)
        .bind(window_key)
        .fetch_one(&self.pool)
        .await?;
        Ok(count.0)
    }

    async fn cleanup_old_windows(&self, before_window_key: &str) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "DELETE FROM rate_limit_state WHERE window_key < ?",
        )
        .bind(before_window_key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
