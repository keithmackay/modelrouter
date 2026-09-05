use async_trait::async_trait;

use crate::db::models::{NewRequestFailure, RequestFailure};
use crate::db::repositories::costs::ArmFilter;

/// Failures recorded for one experiment run, and how many of them were
/// stamped with each variant (a run can fail under more than one).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExperimentRunFailures {
    pub user_id: i64,
    pub correlation_id: String,
    /// Variant on the failure rows; `None` when the stamp is missing.
    pub variant: Option<String>,
    pub failures: i64,
    /// `created_at` of the earliest and latest of these rows (RFC3339), so a
    /// run with no ledger rows still has a span.
    pub first_at: String,
    pub last_at: String,
}

/// Persistence for requests that failed.
///
/// The success path writes a `prompts` row; this is its counterpart, so that
/// "what happened to my request" has an answer in the router for every request,
/// not only the ones that worked.
#[async_trait]
pub trait FailureRepository: Send + Sync {
    async fn create(&self, failure: NewRequestFailure) -> anyhow::Result<RequestFailure>;
    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<RequestFailure>>;
    async fn count(&self) -> anyhow::Result<i64>;
    /// Failure counts grouped by stage, newest window first — the shape an
    /// operator actually wants ("what is failing, and where").
    async fn count_by_stage(&self) -> anyhow::Result<Vec<(String, i64)>>;
    /// Failures for one comparison arm within `[start, end)`. Model arms match
    /// `COALESCE(routed_model, request_model)` because a request that failed
    /// before routing has no routed model.
    async fn count_for_arm(&self, filter: &ArmFilter, start: &str, end: &str) -> anyhow::Result<i64>;
    /// Whether any failure was recorded for this user and correlation id —
    /// enough for a run with no ledger rows to still count as having happened.
    async fn has_rows_for_user(&self, user_id: i64, correlation_id: &str) -> anyhow::Result<bool>;
    /// Failure counts per `(user_id, correlation_id, variant)` for every
    /// failure row stamped with the experiment, unpaginated. Rows without a
    /// user id or correlation id cannot belong to a run and are skipped.
    async fn experiment_run_failures(
        &self,
        experiment_id: i64,
    ) -> anyhow::Result<Vec<ExperimentRunFailures>>;
}
