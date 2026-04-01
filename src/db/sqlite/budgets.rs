use async_trait::async_trait;

use crate::db::models::{BudgetRule, NewBudgetRule};
use crate::db::repositories::budgets::BudgetRepository;
use super::{SqliteDb, now_utc};

#[async_trait]
impl BudgetRepository for SqliteDb {
    async fn list_for_user(&self, user_id: i64) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, created_at, updated_at
               FROM budget_rules WHERE user_id = ? ORDER BY id"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_group(&self, group_name: &str) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, created_at, updated_at
               FROM budget_rules WHERE group_name = ? ORDER BY id"#,
        )
        .bind(group_name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_all(&self) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, created_at, updated_at
               FROM budget_rules ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn create(&self, rule: NewBudgetRule) -> anyhow::Result<BudgetRule> {
        let now = now_utc();
        let model_allow_json = serde_json::to_string(&rule.model_allow).unwrap_or_else(|_| "[]".to_string());
        let model_deny_json = serde_json::to_string(&rule.model_deny).unwrap_or_else(|_| "[]".to_string());
        let result = sqlx::query(
            r#"INSERT INTO budget_rules (user_id, group_name, window, limit_usd, limit_tokens,
                                         model_allow, model_deny, rate_rpm, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(rule.user_id)
        .bind(&rule.group_name)
        .bind(&rule.window)
        .bind(rule.limit_usd)
        .bind(rule.limit_tokens)
        .bind(&model_allow_json)
        .bind(&model_deny_json)
        .bind(rule.rate_rpm)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, created_at, updated_at
               FROM budget_rules WHERE id = ?"#,
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn delete(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM budget_rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
