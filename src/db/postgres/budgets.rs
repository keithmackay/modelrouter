#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{BudgetRule, BudgetScope, NewBudgetRule, UpdateBudgetRule};
use crate::db::repositories::budgets::BudgetRepository;
use super::{PostgresDb, now_utc};

// `window` is a reserved word in Postgres, so it is quoted wherever it is named.
const BUDGET_COLS: &str =
    r#"id, user_id, group_name, api_key_id, tag, "window", limit_usd, limit_tokens,
       model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at,
       project, window_start, window_end"#;

#[async_trait]
impl BudgetRepository for PostgresDb {
    async fn list_for_user(&self, user_id: i64) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(&format!(
            "SELECT {BUDGET_COLS} FROM budget_rules WHERE user_id = $1 ORDER BY id"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_group(&self, group_name: &str) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(&format!(
            "SELECT {BUDGET_COLS} FROM budget_rules WHERE group_name = $1 ORDER BY id"
        ))
        .bind(group_name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_key(&self, api_key_id: i64) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(&format!(
            "SELECT {BUDGET_COLS} FROM budget_rules WHERE api_key_id = $1 ORDER BY id"
        ))
        .bind(api_key_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_all(&self) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(&format!(
            "SELECT {BUDGET_COLS} FROM budget_rules ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn create(&self, rule: NewBudgetRule) -> anyhow::Result<BudgetRule> {
        let now = now_utc();
        let model_allow_json = serde_json::to_string(&rule.model_allow)?;
        let model_deny_json = serde_json::to_string(&rule.model_deny)?;
        let row = sqlx::query_as::<_, BudgetRule>(&format!(
            r#"INSERT INTO budget_rules (user_id, group_name, api_key_id, tag, "window", limit_usd, limit_tokens,
                                         model_allow, model_deny, rate_rpm, max_concurrent, project, window_start, window_end,
                                         created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
               RETURNING {BUDGET_COLS}"#
        ))
        .bind(rule.user_id)
        .bind(&rule.group_name)
        .bind(rule.api_key_id)
        .bind(&rule.tag)
        .bind(&rule.window)
        .bind(rule.limit_usd)
        .bind(rule.limit_tokens)
        .bind(&model_allow_json)
        .bind(&model_deny_json)
        .bind(rule.rate_rpm)
        .bind(rule.max_concurrent)
        .bind(&rule.project)
        .bind(&rule.window_start)
        .bind(&rule.window_end)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list_for_tag(&self, tag: &str) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(&format!(
            "SELECT {BUDGET_COLS} FROM budget_rules WHERE tag = $1"
        ))
        .bind(tag)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM budget_rules WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_for_scope(&self, scope: &BudgetScope) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = match scope {
            BudgetScope::Global => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {BUDGET_COLS} FROM budget_rules \
                     WHERE user_id IS NULL AND group_name IS NULL AND project IS NULL AND api_key_id IS NULL \
                     ORDER BY id"
                ))
                .fetch_all(&self.pool)
                .await?
            }
            BudgetScope::Project(name) => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {BUDGET_COLS} FROM budget_rules WHERE project = $1 ORDER BY id"
                ))
                .bind(name)
                .fetch_all(&self.pool)
                .await?
            }
            BudgetScope::User(user_id) => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {BUDGET_COLS} FROM budget_rules WHERE user_id = $1 ORDER BY id"
                ))
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?
            }
            BudgetScope::Group(group_name) => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {BUDGET_COLS} FROM budget_rules WHERE group_name = $1 ORDER BY id"
                ))
                .bind(group_name)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows)
    }

    async fn update(&self, id: i64, changes: &UpdateBudgetRule) -> anyhow::Result<BudgetRule> {
        let now = now_utc();
        let model_allow = changes.model_allow.as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let model_deny = changes.model_deny.as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let row = sqlx::query_as::<_, BudgetRule>(&format!(
            r#"UPDATE budget_rules SET
                limit_usd = COALESCE($1, limit_usd),
                limit_tokens = COALESCE($2, limit_tokens),
                model_allow = COALESCE($3, model_allow),
                model_deny = COALESCE($4, model_deny),
                rate_rpm = COALESCE($5, rate_rpm),
                max_concurrent = COALESCE($6, max_concurrent),
                window_start = COALESCE($7, window_start),
                window_end = COALESCE($8, window_end),
                updated_at = $9
               WHERE id = $10
               RETURNING {BUDGET_COLS}"#
        ))
        .bind(changes.limit_usd)
        .bind(changes.limit_tokens)
        .bind(&model_allow)
        .bind(&model_deny)
        .bind(changes.rate_rpm)
        .bind(changes.max_concurrent)
        .bind(&changes.window_start)
        .bind(&changes.window_end)
        .bind(&now)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or_else(|| anyhow::anyhow!("budget rule {id} not found"))
    }
}
