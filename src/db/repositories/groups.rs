use async_trait::async_trait;
use crate::db::models::{Group, GroupMembership};

#[async_trait]
pub trait GroupRepository: Send + Sync {
    async fn list_groups(&self) -> anyhow::Result<Vec<Group>>;
    async fn get_group(&self, id: i64) -> anyhow::Result<Option<Group>>;
    async fn find_group_by_name(&self, name: &str) -> anyhow::Result<Option<Group>>;
    async fn create_group(&self, name: &str, priority: i64) -> anyhow::Result<Group>;
    /// Set enabled flag. When `enabled = false`, also disables all active memberships
    /// in a single transaction.
    async fn set_group_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn list_memberships(&self, group_id: i64) -> anyhow::Result<Vec<GroupMembership>>;
    async fn find_active_membership(
        &self,
        group_id: i64,
        user_id: i64,
    ) -> anyhow::Result<Option<GroupMembership>>;
    async fn add_member(&self, group_id: i64, user_id: i64) -> anyhow::Result<GroupMembership>;
    async fn disable_membership(&self, membership_id: i64) -> anyhow::Result<()>;
    async fn set_group_priority(&self, id: i64, priority: i64) -> anyhow::Result<()>;
}
