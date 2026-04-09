use async_trait::async_trait;
use crate::db::models::{User, NewUser};

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<User>>;
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<User>>;
    async fn list(&self) -> anyhow::Result<Vec<User>>;
    async fn create(&self, user: NewUser) -> anyhow::Result<User>;
    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn reset_spend(&self, user_id: i64) -> anyhow::Result<()>;
}
