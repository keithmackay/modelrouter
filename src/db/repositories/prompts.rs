use async_trait::async_trait;
use crate::db::models::{Prompt, NewPrompt};
use crate::db::repositories::costs::ArmFilter;

/// Latency distribution for one comparison arm over a window.
///
/// Only rows with a recorded, positive `latency_ms` count as samples: cache
/// hits are logged with `0` (or `NULL`) and would otherwise drag the
/// percentiles toward zero. Percentiles use the nearest-rank method, so `p50`
/// and `p95` are always actual observed values; all three are `None` when
/// there are no samples.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct LatencySummary {
    pub samples: i64,
    pub mean_ms: Option<f64>,
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
}

impl LatencySummary {
    /// Row offset for the nearest-rank percentile `q` (0.0..=1.0) of `n`
    /// ascending samples: `ceil(n * q) - 1`, clamped to `[0, n - 1]`.
    pub fn nearest_rank_offset(n: i64, q: f64) -> i64 {
        if n <= 0 {
            return 0;
        }
        let rank = (n as f64 * q).ceil() as i64;
        (rank - 1).clamp(0, n - 1)
    }
}

#[async_trait]
pub trait PromptRepository: Send + Sync {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt>;
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<Prompt>>;
    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>>;
    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Prompt>>;
    async fn count(&self) -> anyhow::Result<i64>;
    /// Delete prompt rows with `created_at` before the RFC3339 cutoff; returns
    /// rows deleted. The caller computes the cutoff (retention policy lives in
    /// config, not in the repository).
    async fn purge_older_than(&self, cutoff_rfc3339: &str) -> anyhow::Result<u64>;
    /// Latency samples, mean and nearest-rank p50/p95 for one comparison arm
    /// within `[start, end)`. Model arms match `routed_model`.
    async fn latency_summary(
        &self,
        filter: &ArmFilter,
        start: &str,
        end: &str,
    ) -> anyhow::Result<LatencySummary>;
}
