use async_trait::async_trait;

use crate::db::models::{CostLedgerEntry, NewCostLedgerEntry};
use crate::db::repositories::costs::CostRepository;
use super::{SqliteDb, now_utc};

#[async_trait]
impl CostRepository for SqliteDb {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project,
                                        tokens_in, tokens_out, cost_usd, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(entry.user_id)
        .bind(entry.prompt_id)
        .bind(&entry.model)
        .bind(&entry.provider)
        .bind(&entry.project)
        .bind(entry.tokens_in)
        .bind(entry.tokens_out)
        .bind(entry.cost_usd)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, CostLedgerEntry>(
            "SELECT id, user_id, prompt_id, model, provider, project,
                    tokens_in, tokens_out, cost_usd, created_at
             FROM cost_ledger WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn sum_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<f64> {
        let row: (f64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger
             WHERE user_id = ? AND created_at >= ?",
        )
        .bind(user_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }
}
