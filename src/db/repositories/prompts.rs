use async_trait::async_trait;
use crate::db::models::{Prompt, NewPrompt};

#[async_trait]
pub trait PromptRepository: Send + Sync {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt>;
    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<Prompt>>;
    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>>;
    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Prompt>>;
    async fn count(&self) -> anyhow::Result<i64>;
}
