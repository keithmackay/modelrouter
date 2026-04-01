use async_trait::async_trait;
use crate::db::models::{Prompt, NewPrompt};

#[async_trait]
pub trait PromptRepository: Send + Sync {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt>;
    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>>;
}
