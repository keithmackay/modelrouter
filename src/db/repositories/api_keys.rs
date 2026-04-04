use async_trait::async_trait;
use crate::db::models::{ApiKey, NewApiKey};

#[async_trait]
pub trait ApiKeyRepository: Send + Sync {
    async fn find_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>>;
    async fn list_api_keys_for_user(&self, user_id: i64) -> anyhow::Result<Vec<ApiKey>>;
    async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey>;
    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<()>;
}
