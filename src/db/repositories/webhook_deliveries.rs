use anyhow::Result;
use async_trait::async_trait;

/// One due delivery claimed for this process to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedDelivery {
    pub id: i64,
    pub webhook_id: i64,
    pub event: String,
    pub payload: String,
    /// Attempts already finished before this claim. The first try is 0.
    pub attempt: i64,
}

/// The durable queue behind usage-webhook delivery.
#[async_trait]
pub trait WebhookDeliveryRepository {
    /// Insert one pending delivery, due at `next_attempt_at`. Returns its id.
    async fn enqueue_delivery(
        &self,
        webhook_id: i64,
        event: &str,
        payload: &str,
        next_attempt_at: &str,
    ) -> Result<i64>;

    /// Claim up to `limit` deliveries that are due at `now` and not held
    /// past it. A claimed row is leased to `worker` until `locked_until`.
    async fn claim_due_deliveries(
        &self,
        worker: &str,
        now: &str,
        locked_until: &str,
        limit: i64,
    ) -> Result<Vec<ClaimedDelivery>>;

    /// Record a successful POST. No-op when this worker no longer holds the lease.
    async fn mark_delivered(&self, id: i64, worker: &str, delivered_at: &str) -> Result<bool>;

    /// Schedule another attempt. No-op when this worker no longer holds the lease.
    async fn schedule_retry(
        &self,
        id: i64,
        worker: &str,
        attempt: i64,
        next_attempt_at: &str,
        last_error: &str,
    ) -> Result<bool>;

    /// Stop retrying. The row stays, with `last_error`, so a dropped delivery
    /// is visible. No-op when this worker no longer holds the lease.
    async fn abandon_delivery(
        &self,
        id: i64,
        worker: &str,
        attempt: i64,
        last_error: &str,
    ) -> Result<bool>;

    /// Put a claimed row back, still due later, without counting an attempt.
    /// Used when the webhook is disabled and may be turned back on.
    async fn release_delivery(
        &self,
        id: i64,
        worker: &str,
        next_attempt_at: &str,
        last_error: &str,
    ) -> Result<bool>;

    /// Delete deliveries that succeeded before `cutoff`. Pending and given-up
    /// rows are left in place.
    async fn purge_delivered_before(&self, cutoff: &str) -> Result<u64>;
}
