use async_trait::async_trait;
use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelSummaryRow {
    pub model: String,
    pub total_cost_usd: f64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub request_count: i64,
}

#[async_trait]
pub trait CostRepository: Send + Sync {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry>;
    async fn sum_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<f64>;
    async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64>;
    async fn sum_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<f64>;
    async fn sum_tokens_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<i64>;
    async fn list_cost_entries_before(&self, cutoff: &str) -> anyhow::Result<Vec<crate::db::models::CostLedgerEntry>>;
    async fn delete_cost_entries_by_ids(&self, ids: &[i64]) -> anyhow::Result<()>;
    /// Sum spend for a user in [start, end) — both ISO 8601 UTC timestamps (inclusive start, exclusive end).
    async fn sum_for_user_between(&self, user_id: i64, start: &str, end: &str) -> anyhow::Result<f64>;
    /// Sum spend for a project since a timestamp (inclusive).
    /// Uses cost_ledger.project column (denormalized at write time).
    async fn sum_for_project_since(&self, project: &str, since: &str) -> anyhow::Result<f64>;
    /// Sum spend for a project in [start, end) (inclusive start, exclusive end).
    /// Uses cost_ledger.project column (denormalized at write time).
    async fn sum_for_project_between(&self, project: &str, start: &str, end: &str) -> anyhow::Result<f64>;
    /// Sum all spend across all users/projects since a timestamp (inclusive).
    async fn sum_global_since(&self, since: &str) -> anyhow::Result<f64>;
    /// Sum all spend across all users/projects in [start, end) (inclusive start, exclusive end).
    async fn sum_global_between(&self, start: &str, end: &str) -> anyhow::Result<f64>;
    /// Return (cost_usd, tokens_in, tokens_out, request_count) for a user since a timestamp.
    async fn user_cost_stats_since(&self, user_id: i64, since: &str) -> anyhow::Result<(f64, i64, i64, i64)>;
    /// Aggregate cost stats grouped by user_id with optional filters.
    /// Returns Vec of (user_id, cost_usd, tokens_in, tokens_out, request_count).
    /// `filter_user_ids`: None = all users; Some(&[]) = no users (empty result).
    /// `since`: ISO 8601 UTC; use "1970-01-01T00:00:00Z" for all-time.
    async fn cost_stats_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, f64, i64, i64, i64)>>;
    /// Distinct non-null project values present in the cost ledger, sorted.
    async fn distinct_projects_in_ledger(&self) -> anyhow::Result<Vec<String>>;
    /// Distinct non-null model values present in the cost ledger, sorted.
    async fn distinct_models_in_ledger(&self) -> anyhow::Result<Vec<String>>;
    /// Daily spend series: returns (date_str, cost_usd) pairs grouped by calendar day.
    /// `filter_user_ids`: None = all users; Some(&[]) = empty result.
    /// `start`/`end`: ISO 8601 UTC timestamps (inclusive start, exclusive end).
    async fn list_daily_spend(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_model: Option<&str>,
        start: &str,
        end: &str,
    ) -> anyhow::Result<Vec<(String, f64)>>;
    /// Aggregate cost stats grouped by model, with optional filters.
    async fn summarize_by_model(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<ModelSummaryRow>>;
    /// Per-row cost stats grouped by (user_id, model, project, api_key_id).
    /// Returns Vec of (user_id, model, project, api_key_id, cost_usd, tokens_in, tokens_out, request_count).
    /// Filters mirror cost_stats_grouped; adds an optional model filter.
    async fn cost_rows_grouped(
        &self,
        filter_user_ids: Option<&[i64]>,
        filter_project: Option<&str>,
        filter_api_key_id: Option<i64>,
        filter_model: Option<&str>,
        since: &str,
    ) -> anyhow::Result<Vec<(i64, String, Option<String>, Option<i64>, f64, i64, i64, i64)>>;
}
