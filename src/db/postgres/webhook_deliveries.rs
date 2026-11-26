#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::repositories::webhook_deliveries::{ClaimedDelivery, WebhookDeliveryRepository};

use super::PostgresDb;

#[derive(sqlx::FromRow)]
struct ClaimedRow {
    id: i64,
    webhook_id: i64,
    event: String,
    payload: String,
    attempt: i32,
}

impl From<ClaimedRow> for ClaimedDelivery {
    fn from(r: ClaimedRow) -> Self {
        Self {
            id: r.id,
            webhook_id: r.webhook_id,
            event: r.event,
            payload: r.payload,
            attempt: i64::from(r.attempt),
        }
    }
}

#[async_trait]
impl WebhookDeliveryRepository for PostgresDb {
    async fn enqueue_delivery(
        &self,
        webhook_id: i64,
        event: &str,
        payload: &str,
        next_attempt_at: &str,
    ) -> anyhow::Result<i64> {
        let created_at = super::now_utc();
        let id = sqlx::query_scalar(
            "INSERT INTO webhook_deliveries \
             (webhook_id, event, payload, attempt, next_attempt_at, created_at) \
             VALUES ($1, $2, $3, 0, $4, $5) \
             RETURNING id",
        )
        .bind(webhook_id)
        .bind(event)
        .bind(payload)
        .bind(next_attempt_at)
        .bind(&created_at)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    async fn claim_due_deliveries(
        &self,
        worker: &str,
        now: &str,
        locked_until: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ClaimedDelivery>> {
        // SKIP LOCKED: a replica sending a row does not block the others,
        // and a lease that has expired is claimable again.
        let rows = sqlx::query_as::<_, ClaimedRow>(
            "WITH due AS ( \
                 SELECT id FROM webhook_deliveries \
                 WHERE delivered_at IS NULL \
                   AND next_attempt_at IS NOT NULL \
                   AND next_attempt_at <= $3 \
                   AND (locked_until IS NULL OR locked_until <= $3) \
                 ORDER BY next_attempt_at, id \
                 LIMIT $4 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             UPDATE webhook_deliveries AS d \
             SET locked_by = $1, locked_until = $2 \
             FROM due \
             WHERE d.id = due.id \
             RETURNING d.id, d.webhook_id, d.event, d.payload, d.attempt",
        )
        .bind(worker)
        .bind(locked_until)
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ClaimedDelivery::from).collect())
    }

    async fn mark_delivered(
        &self,
        id: i64,
        worker: &str,
        delivered_at: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET delivered_at = $1, next_attempt_at = NULL, locked_by = NULL, \
                 locked_until = NULL, last_error = NULL \
             WHERE id = $2 AND locked_by = $3",
        )
        .bind(delivered_at)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn schedule_retry(
        &self,
        id: i64,
        worker: &str,
        attempt: i64,
        next_attempt_at: &str,
        last_error: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET attempt = $1, next_attempt_at = $2, last_error = $3, \
                 locked_by = NULL, locked_until = NULL \
             WHERE id = $4 AND locked_by = $5",
        )
        .bind(i32::try_from(attempt).unwrap_or(i32::MAX))
        .bind(next_attempt_at)
        .bind(last_error)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn abandon_delivery(
        &self,
        id: i64,
        worker: &str,
        attempt: i64,
        last_error: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET attempt = $1, next_attempt_at = NULL, last_error = $2, \
                 locked_by = NULL, locked_until = NULL \
             WHERE id = $3 AND locked_by = $4",
        )
        .bind(i32::try_from(attempt).unwrap_or(i32::MAX))
        .bind(last_error)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn release_delivery(
        &self,
        id: i64,
        worker: &str,
        next_attempt_at: &str,
        last_error: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET next_attempt_at = $1, last_error = $2, locked_by = NULL, locked_until = NULL \
             WHERE id = $3 AND locked_by = $4",
        )
        .bind(next_attempt_at)
        .bind(last_error)
        .bind(id)
        .bind(worker)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn purge_delivered_before(&self, cutoff: &str) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "DELETE FROM webhook_deliveries \
             WHERE delivered_at IS NOT NULL AND delivered_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
