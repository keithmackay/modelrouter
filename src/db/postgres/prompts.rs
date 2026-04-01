#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{NewPrompt, Prompt};
use crate::db::repositories::prompts::PromptRepository;
use super::{PostgresDb, now_utc};

#[async_trait]
impl PromptRepository for PostgresDb {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt> {
        let now = now_utc();
        let row = sqlx::query_as::<_, Prompt>(
            r#"INSERT INTO prompts (
                user_id, session_id, request_model, routed_model, provider,
                messages, response, finish_reason, prompt_tokens, completion_tokens,
                cost_usd, latency_ms, tags, project, created_at
               ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
               RETURNING id, user_id, session_id, request_model, routed_model, provider,
                         messages, response, finish_reason, prompt_tokens, completion_tokens,
                         cost_usd, latency_ms, tags, project, created_at"#,
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
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<Prompt>> {
        let row = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cost_usd, latency_ms, tags, project, created_at
               FROM prompts WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cost_usd, latency_ms, tags, project, created_at
               FROM prompts WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2"#,
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(
            r#"SELECT id, user_id, session_id, request_model, routed_model, provider,
                      messages, response, finish_reason, prompt_tokens, completion_tokens,
                      cost_usd, latency_ms, tags, project, created_at
               FROM prompts ORDER BY created_at DESC LIMIT $1 OFFSET $2"#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn count(&self) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM prompts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }
}
