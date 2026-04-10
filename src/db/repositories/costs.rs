use async_trait::async_trait;
use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};

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
}
