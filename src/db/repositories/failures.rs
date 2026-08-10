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
}
