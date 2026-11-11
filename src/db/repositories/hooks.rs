use async_trait::async_trait;
use crate::db::models::HookMetric;

#[async_trait]
pub trait HookRepository: Send + Sync {
    async fn has_permission(&self, hook_name: &str, capability: &str) -> anyhow::Result<bool>;
    async fn grant_permission(&self, hook_name: &str, capability: &str, granted_by: Option<i64>) -> anyhow::Result<()>;
    async fn revoke_permission(&self, hook_name: &str, capability: &str) -> anyhow::Result<()>;
    async fn record_metric(&self, metric: HookMetric) -> anyhow::Result<()>;
    async fn list_metrics(&self, hook_name: &str) -> anyhow::Result<Vec<HookMetric>>;
    /// Delete metric rows with `invoked_at` before `cutoff_rfc3339` (the
    /// retention tick). Returns the number deleted.
    async fn purge_metrics_older_than(&self, cutoff_rfc3339: &str) -> anyhow::Result<u64>;
}
