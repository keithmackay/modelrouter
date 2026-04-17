use async_trait::async_trait;
use crate::db::models::{Model, ModelFailover, NewModel};

#[async_trait]
pub trait ModelRepository: Send + Sync {
    async fn create_model(&self, model: NewModel) -> anyhow::Result<Model>;
    async fn list_models(&self) -> anyhow::Result<Vec<Model>>;
    async fn get_model(&self, id: i64) -> anyhow::Result<Option<Model>>;
    async fn set_model_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn delete_model(&self, id: i64) -> anyhow::Result<bool>;

    /// Replace the entire failover chain for `primary_model` with `fallbacks` (ordered).
    async fn set_failovers(&self, primary_model: &str, fallbacks: &[String]) -> anyhow::Result<()>;
    /// Return failover targets for `primary_model`, ordered by priority ascending.
    async fn list_failovers(&self, primary_model: &str) -> anyhow::Result<Vec<ModelFailover>>;
    /// Return all failover rows (used by routing layer to build full map at startup).
    async fn list_all_failovers(&self) -> anyhow::Result<Vec<ModelFailover>>;
}
