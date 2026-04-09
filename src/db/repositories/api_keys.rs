use async_trait::async_trait;
use crate::db::models::{ApiKey, NewApiKey};

#[async_trait]
pub trait ApiKeyRepository: Send + Sync {
    async fn find_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>>;
    async fn list_api_keys_for_user(&self, user_id: i64) -> anyhow::Result<Vec<ApiKey>>;
    async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey>;
    async fn list_all_api_keys(&self) -> anyhow::Result<Vec<ApiKey>>;
    async fn set_key_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn disable_all_keys_for_user(&self, user_id: i64) -> anyhow::Result<()>;
    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<()>;
    /// Find any key (enabled or disabled) for the given user+project combo.
    async fn find_key_by_user_project(&self, user_id: i64, project: Option<&str>) -> anyhow::Result<Option<ApiKey>>;
}
