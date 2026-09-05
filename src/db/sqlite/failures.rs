use async_trait::async_trait;

use crate::db::models::{NewRequestFailure, RequestFailure};
use crate::db::repositories::failures::FailureRepository;
use super::{SqliteDb, now_utc};

/// Columns selected when reading a failure row back.
const FAILURE_COLUMNS: &str = "id, user_id, api_key_id, endpoint, request_model, routed_model, \
                               provider, stage, status_code, error_message, attempts, latency_ms, \
                               project, attribution_correlation_id, attribution_tags, created_at";

#[async_trait]
impl FailureRepository for SqliteDb {
    async fn create(&self, failure: NewRequestFailure) -> anyhow::Result<RequestFailure> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO request_failures (
                user_id, api_key_id, endpoint, request_model, routed_model, provider,
                stage, status_code, error_message, attempts, latency_ms, project,
                attribution_correlation_id, attribution_tags, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(failure.user_id)
        .bind(failure.api_key_id)
        .bind(&failure.endpoint)
        .bind(&failure.request_model)
        .bind(&failure.routed_model)
        .bind(&failure.provider)
        .bind(failure.stage.as_str())
        .bind(failure.status_code)
        .bind(&failure.error_message)
        .bind(failure.attempts)
        .bind(failure.latency_ms)
        .bind(&failure.project)
        .bind(&failure.attribution_correlation_id)
        .bind(&failure.attribution_tags)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures WHERE id = ?",
            FAILURE_COLUMNS
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<RequestFailure>> {
        let rows = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures ORDER BY created_at DESC LIMIT ? OFFSET ?",
            FAILURE_COLUMNS
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn count(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM request_failures")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    async fn count_by_stage(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT stage, COUNT(*) FROM request_failures GROUP BY stage ORDER BY COUNT(*) DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<RequestFailure>> {
        let row = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures WHERE id = ?",
            FAILURE_COLUMNS
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn find_by_correlation_id(&self, correlation_id: &str) -> anyhow::Result<Vec<RequestFailure>> {
        let rows = sqlx::query_as::<_, RequestFailure>(&format!(
            "SELECT {} FROM request_failures WHERE attribution_correlation_id = ? ORDER BY created_at DESC",
            FAILURE_COLUMNS
        ))
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
