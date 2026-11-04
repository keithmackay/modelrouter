use async_trait::async_trait;

use crate::db::models::LearnedModelCapability;

/// Model capabilities learned from provider rejections. Loaded into
/// [`crate::router::learned_capabilities::LearnedCapabilities`] at startup and
/// written through whenever the router learns or an operator clears one.
#[async_trait]
pub trait LearnedCapabilityRepository: Send + Sync {
    async fn list_learned_capabilities(&self) -> anyhow::Result<Vec<LearnedModelCapability>>;
    /// Insert the row, or replace it if the model already has one.
    async fn upsert_learned_capability(&self, row: &LearnedModelCapability) -> anyhow::Result<()>;
    /// Returns true when a row was removed.
    async fn delete_learned_capability(&self, model: &str) -> anyhow::Result<bool>;
}
