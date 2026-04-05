#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};
use crate::db::repositories::costs::CostRepository;
use super::{PostgresDb, now_utc};

#[async_trait]
impl CostRepository for PostgresDb {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        let row = sqlx::query_as::<_, CostLedgerEntry>(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, api_key_id, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
               RETURNING id, user_id, prompt_id, model, provider, project,
                         tokens_in, tokens_out, cost_usd, created_at, api_key_id"#,
        )
        .bind(entry.user_id)
        .bind(entry.prompt_id)
        .bind(&entry.model)
        .bind(&entry.provider)
        .bind(&entry.project)
        .bind(entry.tokens_in)
        .bind(entry.tokens_out)
        .bind(entry.cost_usd)
        .bind(entry.api_key_id)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn sum_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger
             WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger
             WHERE user_id = $1 AND created_at >= $2",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger WHERE api_key_id = $1 AND created_at >= $2"
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn sum_tokens_for_key_since(&self, api_key_id: i64, since: &str) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger WHERE api_key_id = $1 AND created_at >= $2"
        )
        .bind(api_key_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    async fn list_cost_entries_before(&self, _cutoff: &str) -> anyhow::Result<Vec<CostLedgerEntry>> {
        Ok(vec![])
    }

    async fn delete_cost_entries_by_ids(&self, _ids: &[i64]) -> anyhow::Result<()> {
        Ok(())
    }
}
