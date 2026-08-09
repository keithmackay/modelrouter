use async_trait::async_trait;

use crate::db::models::{ModelAlias, NewModelAlias};

/// Runtime-managed model aliases (issue #9).
///
/// These are the operational source of truth for alias -> target routing:
/// they override aliases derived from registered model rows, which in turn
/// override config-file `routing.model_aliases`.
#[async_trait]
pub trait AliasRepository: Send + Sync {
    async fn list_aliases(&self) -> anyhow::Result<Vec<ModelAlias>>;
    async fn get_alias(&self, alias: &str) -> anyhow::Result<Option<ModelAlias>>;
    /// Create the alias, or replace its target if it already exists.
    async fn upsert_alias(&self, alias: NewModelAlias) -> anyhow::Result<ModelAlias>;
    /// Returns true when a row was removed.
    async fn delete_alias(&self, alias: &str) -> anyhow::Result<bool>;
}
