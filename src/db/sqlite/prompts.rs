use async_trait::async_trait;

use crate::db::models::{NewPrompt, Prompt};
use crate::db::repositories::prompts::PromptRepository;
use super::{SqliteDb, now_utc};

#[async_trait]
impl PromptRepository for SqliteDb {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO prompts (
                user_id, session_id, request_model, routed_model, provider,
                messages, response, finish_reason, prompt_tokens, completion_tokens,
                cost_usd, latency_ms, tags, project, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(prompt.user_id)
        .bind(prompt.session_id)
        .bind(&prompt.request_model)
        .bind(&prompt.routed_model)
        .bind(&prompt.provider)
        .bind(&prompt.messages)
        .bind(&prompt.response)
        .bind(&prompt.finish_reason)
        .bind(prompt.prompt_tokens)
        .bind(prompt.completion_tokens)
        .bind(prompt.cost_usd)
        .bind(prompt.latency_ms)
        .bind(&prompt.tags)
        .bind(&prompt.project)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cost_usd, latency_ms, tags, project, created_at
               FROM prompts WHERE id = ?"#,
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cost_usd, latency_ms, tags, project, created_at
               FROM prompts WHERE user_id = ? ORDER BY created_at DESC LIMIT ?"#,
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
