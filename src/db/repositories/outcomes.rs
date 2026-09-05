use async_trait::async_trait;

use crate::db::models::{NewRunOutcome, RunOutcome};

/// Per-run outcome feedback (spec §7a, `POST /v1/feedback`).
///
/// One row per `(user_id, attribution_correlation_id)`: a later report for
/// the same run replaces the earlier one.
#[async_trait]
pub trait OutcomeRepository: Send + Sync {
    /// Insert the outcome, or replace every mutable field of the existing row
    /// for the same user and correlation id.
    async fn upsert(&self, outcome: NewRunOutcome) -> anyhow::Result<RunOutcome>;
    async fn get(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<Option<RunOutcome>>;
    async fn for_experiment(&self, experiment_id: i64) -> anyhow::Result<Vec<RunOutcome>>;
    /// Null out `note` on every outcome stamped with the experiment; the rest
    /// of the row stays. Returns the number of rows touched.
    async fn clear_notes(&self, experiment_id: i64) -> anyhow::Result<u64>;
}
