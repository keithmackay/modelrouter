use async_trait::async_trait;
use crate::db::models::{AdminUser, NewAdminUser};

#[async_trait]
pub trait AdminUserRepository: Send + Sync {
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<AdminUser>>;
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<AdminUser>>;
    async fn list(&self) -> anyhow::Result<Vec<AdminUser>>;
    async fn create(&self, user: NewAdminUser) -> anyhow::Result<AdminUser>;
    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn update_last_login(&self, id: i64) -> anyhow::Result<()>;
}
