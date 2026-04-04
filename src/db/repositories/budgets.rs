use async_trait::async_trait;
use crate::db::models::{BudgetRule, NewBudgetRule};

#[async_trait]
pub trait BudgetRepository: Send + Sync {
    async fn list_for_user(&self, user_id: i64) -> anyhow::Result<Vec<BudgetRule>>;
    async fn list_for_group(&self, group_name: &str) -> anyhow::Result<Vec<BudgetRule>>;
    async fn list_for_key(&self, api_key_id: i64) -> anyhow::Result<Vec<BudgetRule>>;
    async fn list_all(&self) -> anyhow::Result<Vec<BudgetRule>>;
    async fn create(&self, rule: NewBudgetRule) -> anyhow::Result<BudgetRule>;
    async fn delete(&self, id: i64) -> anyhow::Result<()>;
}
