use async_trait::async_trait;

use crate::db::repositories::webhook_deliveries::{ClaimedDelivery, WebhookDeliveryRepository};

use super::SqliteDb;

#[derive(sqlx::FromRow)]
struct ClaimedRow {
    id: i64,
    webhook_id: i64,
    event: String,
    payload: String,
    attempt: i64,
}

impl From<ClaimedRow> for ClaimedDelivery {
    fn from(r: ClaimedRow) -> Self {
        Self {
            id: r.id,
            webhook_id: r.webhook_id,
            event: r.event,
            payload: r.payload,
            attempt: r.attempt,
        }
    }
}

#[async_trait]
impl WebhookDeliveryRepository for SqliteDb {
    async fn enqueue_delivery(
        &self,
        webhook_id: i64,
        event: &str,
        payload: &str,
        next_attempt_at: &str,
    ) -> anyhow::Result<i64> {
        let created_at = super::now_utc();
        let result = sqlx::query(
            "INSERT INTO webhook_deliveries \
             (webhook_id, event, payload, attempt, next_attempt_at, created_at) \
             VALUES (?, ?, ?, 0, ?, ?)",
        )
        .bind(webhook_id)
        .bind(event)
        .bind(payload)
        .bind(next_attempt_at)
        .bind(&created_at)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn claim_due_deliveries(
        &self,
        worker: &str,
        now: &str,
        locked_until: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ClaimedDelivery>> {
        // One writer at a time. Each statement leases a single due row, so a
        // crash between statements leaves the rest for the next claim, and a
        // row already leased past `now` is skipped until that lease expires.
        let mut claimed = Vec::new();
        for _ in 0..limit.max(0) {
            let row = sqlx::query_as::<_, ClaimedRow>(
                "UPDATE webhook_deliveries \
                 SET locked_by = ?, locked_until = ? \
                 WHERE id = ( \
                     SELECT id FROM webhook_deliveries \
                     WHERE delivered_at IS NULL \
                       AND next_attempt_at IS NOT NULL \
                       AND next_attempt_at <= ? \
                       AND (locked_until IS NULL OR locked_until <= ?) \
                     ORDER BY next_attempt_at, id \
                     LIMIT 1 \
                 ) \
                 RETURNING id, webhook_id, event, payload, attempt",
            )
            .bind(worker)
            .bind(locked_until)
            .bind(now)
            .bind(now)
            .fetch_optional(&self.pool)
            .await?;
            match row {
                Some(row) => claimed.push(ClaimedDelivery::from(row)),
                None => break,
            }
        }
        Ok(claimed)
    }

    async fn mark_delivered(
        &self,
        id: i64,
        worker: &str,
        delivered_at: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET delivered_at = ?, next_attempt_at = NULL, locked_by = NULL, \
                 locked_until = NULL, last_error = NULL \
             WHERE id = ? AND locked_by = ?",
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
             SET attempt = ?, next_attempt_at = ?, last_error = ?, \
                 locked_by = NULL, locked_until = NULL \
             WHERE id = ? AND locked_by = ?",
        )
        .bind(attempt)
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
             SET attempt = ?, next_attempt_at = NULL, last_error = ?, \
                 locked_by = NULL, locked_until = NULL \
             WHERE id = ? AND locked_by = ?",
        )
        .bind(attempt)
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
             SET next_attempt_at = ?, last_error = ?, locked_by = NULL, locked_until = NULL \
             WHERE id = ? AND locked_by = ?",
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
            "DELETE FROM webhook_deliveries WHERE delivered_at IS NOT NULL AND delivered_at < ?",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
