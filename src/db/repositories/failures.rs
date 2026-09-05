use async_trait::async_trait;

use crate::db::models::{NewRequestFailure, RequestFailure};

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
    /// Retrieve a single failure by its primary key, for detail-drill surfaces
    /// a downstream caller may build.
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<RequestFailure>>;
    /// List all failures matching a given correlation id, enabling drill-down
    /// from a downstream caller's trace.
    async fn find_by_correlation_id(&self, correlation_id: &str) -> anyhow::Result<Vec<RequestFailure>>;
}
