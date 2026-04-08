use async_trait::async_trait;
use crate::db::models::{AdminUser, NewAdminUser, NewAdminUserFromOidc};

#[async_trait]
pub trait AdminUserRepository: Send + Sync {
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<AdminUser>>;
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<AdminUser>>;
    async fn list(&self) -> anyhow::Result<Vec<AdminUser>>;
    async fn create(&self, user: NewAdminUser) -> anyhow::Result<AdminUser>;
    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn delete(&self, id: i64) -> anyhow::Result<()>;
    async fn update_last_login(&self, id: i64) -> anyhow::Result<()>;
    async fn find_by_oidc_subject(&self, subject: &str) -> anyhow::Result<Option<AdminUser>>;
    async fn create_from_oidc(&self, user: NewAdminUserFromOidc) -> anyhow::Result<AdminUser>;
    async fn update_password_hash(&self, id: i64, hash: &str) -> anyhow::Result<()>;
}
