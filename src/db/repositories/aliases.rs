use async_trait::async_trait;

use crate::db::models::{ModelAlias, NewModelAlias, NewScopedAlias, ScopedAlias};

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

    /// Every scoped alias override, expired or not, ordered by scope and alias.
    async fn list_scoped_aliases(&self) -> anyhow::Result<Vec<ScopedAlias>>;
    /// Replace one scope's whole set of overrides in a single transaction: an
    /// empty `aliases` clears the scope. Returns the rows now stored for it.
    async fn replace_scoped_aliases(
        &self,
        tag_key: &str,
        tag_value: &str,
        aliases: &[NewScopedAlias],
        expires_at: i64,
        created_by: Option<&str>,
    ) -> anyhow::Result<Vec<ScopedAlias>>;
    /// Delete every override whose expiry has passed; returns the deleted rows.
    async fn delete_expired_scoped_aliases(&self, now_epoch: i64) -> anyhow::Result<Vec<ScopedAlias>>;
}
