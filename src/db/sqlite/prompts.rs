use async_trait::async_trait;

use crate::db::models::{NewPrompt, Prompt};
use crate::db::repositories::prompts::PromptRepository;
use super::{SqliteDb, now_utc};

/// Columns selected when reading a prompt row back.
const PROMPT_COLUMNS: &str = "id, user_id, session_id, request_model, routed_model, provider, \
                              messages, response, finish_reason, prompt_tokens, completion_tokens, \
                              cache_read_tokens, cache_write_tokens, cost_usd, latency_ms, tags, \
                              project, attribution_correlation_id, attribution_tags, created_at";

#[async_trait]
impl PromptRepository for SqliteDb {
    async fn create(&self, prompt: NewPrompt) -> anyhow::Result<Prompt> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO prompts (
                user_id, session_id, request_model, routed_model, provider,
                messages, response, finish_reason, prompt_tokens, completion_tokens,
                cache_read_tokens, cache_write_tokens,
                cost_usd, latency_ms, tags, project,
                attribution_correlation_id, attribution_tags, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        .bind(prompt.cache_read_tokens)
        .bind(prompt.cache_write_tokens)
        .bind(prompt.cost_usd)
        .bind(prompt.latency_ms)
        .bind(&prompt.tags)
        .bind(&prompt.project)
        .bind(&prompt.attribution_correlation_id)
        .bind(&prompt.attribution_tags)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts WHERE id = ?",
            PROMPT_COLUMNS
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<Prompt>> {
        let row = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts WHERE id = ?",
            PROMPT_COLUMNS
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_by_user(&self, user_id: i64, limit: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts WHERE user_id = ? ORDER BY created_at DESC LIMIT ?",
            PROMPT_COLUMNS
        ))
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<Prompt>> {
        let rows = sqlx::query_as::<_, Prompt>(&format!(
            "SELECT {} FROM prompts ORDER BY created_at DESC LIMIT ? OFFSET ?",
            PROMPT_COLUMNS
        ))
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

    async fn purge_older_than(&self, cutoff_rfc3339: &str) -> anyhow::Result<u64> {
        // created_at is uniformly RFC3339 UTC (now_utc), so string comparison
        // orders correctly — no parsing needed at purge time.
        let result = sqlx::query("DELETE FROM prompts WHERE created_at < ?")
            .bind(cutoff_rfc3339)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    async fn make_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, name, created_at) VALUES (1, 'test', '2025-01-01T00:00:00Z')")
            .execute(&db.pool)
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn cache_tokens_round_trip_and_flag_cached() {
        let db = make_db().await;
        let saved = PromptRepository::create(
            &db,
            NewPrompt {
                user_id: 1,
                session_id: None,
                request_model: "claude-sonnet-4-6".to_string(),
                routed_model: "anthropic/claude-sonnet-4-6".to_string(),
                provider: "anthropic".to_string(),
                messages: "[]".to_string(),
                response: None,
                finish_reason: None,
                prompt_tokens: 100,
                completion_tokens: 50,
                cache_read_tokens: 900,
                cache_write_tokens: 0,
                cost_usd: 0.01,
                latency_ms: None,
                tags: "[]".to_string(),
                project: None,
                attribution_correlation_id: None,
                attribution_tags: "{}".to_string(),
            },
        )
        .await
        .unwrap();

        assert_eq!(saved.cache_read_tokens, 900);
        assert_eq!(saved.cache_write_tokens, 0);
        assert!(saved.is_cached());

        let fetched = PromptRepository::find_by_id(&db, saved.id).await.unwrap().unwrap();
        assert_eq!(fetched.cache_read_tokens, 900);
        assert!(fetched.is_cached());
    }

    #[tokio::test]
    async fn no_cache_tokens_means_not_cached() {
        let db = make_db().await;
        let saved = PromptRepository::create(
            &db,
            NewPrompt {
                user_id: 1,
                session_id: None,
                request_model: "gpt-4o".to_string(),
                routed_model: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
                messages: "[]".to_string(),
                response: None,
                finish_reason: None,
                prompt_tokens: 100,
                completion_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.01,
                latency_ms: None,
                tags: "[]".to_string(),
                project: None,
                attribution_correlation_id: None,
                attribution_tags: "{}".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(!saved.is_cached());
    }
}
