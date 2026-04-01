use async_trait::async_trait;
use crate::db::models::{User, NewUser};

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn find_by_api_key(&self, key_hash: &str) -> anyhow::Result<Option<User>>;
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<User>>;
    async fn list(&self) -> anyhow::Result<Vec<User>>;
    async fn create(&self, user: NewUser) -> anyhow::Result<User>;
    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn rotate_key(&self, id: i64, new_key_hash: &str, overlap_expires_at: &str) -> anyhow::Result<()>;
    async fn expire_old_keys(&self) -> anyhow::Result<u64>;
}
