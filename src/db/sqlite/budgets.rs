use async_trait::async_trait;

use crate::db::models::{BudgetRule, BudgetScope, NewBudgetRule, UpdateBudgetRule};
use crate::db::repositories::budgets::BudgetRepository;
use super::{SqliteDb, now_utc};

#[async_trait]
impl BudgetRepository for SqliteDb {
    async fn list_for_user(&self, user_id: i64) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at,
                      project, window_start, window_end
               FROM budget_rules WHERE user_id = ? ORDER BY id"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_group(&self, group_name: &str) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at,
                      project, window_start, window_end
               FROM budget_rules WHERE group_name = ? ORDER BY id"#,
        )
        .bind(group_name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_key(&self, api_key_id: i64) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at,
                      project, window_start, window_end
               FROM budget_rules WHERE api_key_id = ? ORDER BY id"#,
        )
        .bind(api_key_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_all(&self) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at,
                      project, window_start, window_end
               FROM budget_rules ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn create(&self, rule: NewBudgetRule) -> anyhow::Result<BudgetRule> {
        let now = now_utc();
        let model_allow = serde_json::to_string(&rule.model_allow)?;
        let model_deny = serde_json::to_string(&rule.model_deny)?;
        let result = sqlx::query(
            r#"INSERT INTO budget_rules
               (user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens,
                model_allow, model_deny, rate_rpm, max_concurrent, project, window_start, window_end,
                created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(rule.user_id)
        .bind(&rule.group_name)
        .bind(rule.api_key_id)
        .bind(&rule.tag)
        .bind(&rule.window)
        .bind(rule.limit_usd)
        .bind(rule.limit_tokens)
        .bind(&model_allow)
        .bind(&model_deny)
        .bind(rule.rate_rpm)
        .bind(rule.max_concurrent)
        .bind(&rule.project)
        .bind(&rule.window_start)
        .bind(&rule.window_end)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        sqlx::query_as::<_, BudgetRule>(
            r#"SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens,
                      model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at,
                      project, window_start, window_end
               FROM budget_rules WHERE id = ?"#,
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    async fn list_for_tag(&self, tag: &str) -> anyhow::Result<Vec<BudgetRule>> {
        let rows = sqlx::query_as::<_, BudgetRule>(
            "SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens, \
             model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at, \
             project, window_start, window_end \
             FROM budget_rules WHERE tag = ?"
        )
        .bind(tag)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn delete(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM budget_rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_for_scope(&self, scope: &BudgetScope) -> anyhow::Result<Vec<BudgetRule>> {
        let cols = "id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens, \
                    model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at, \
                    project, window_start, window_end";
        let rows = match scope {
            BudgetScope::Global => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {} FROM budget_rules \
                     WHERE user_id IS NULL AND group_name IS NULL AND project IS NULL AND api_key_id IS NULL \
                     ORDER BY id", cols
                ))
                .fetch_all(&self.pool)
                .await?
            }
            BudgetScope::Project(name) => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {} FROM budget_rules WHERE project = ? ORDER BY id", cols
                ))
                .bind(name)
                .fetch_all(&self.pool)
                .await?
            }
            BudgetScope::User(user_id) => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {} FROM budget_rules WHERE user_id = ? ORDER BY id", cols
                ))
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?
            }
            BudgetScope::Group(group_name) => {
                sqlx::query_as::<_, BudgetRule>(&format!(
                    "SELECT {} FROM budget_rules WHERE group_name = ? ORDER BY id", cols
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
            .map(|v| serde_json::to_string(v))
            .transpose()?;
        let model_deny = changes.model_deny.as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;

        let result = sqlx::query(
            r#"UPDATE budget_rules SET
                limit_usd = COALESCE(?, limit_usd),
                limit_tokens = COALESCE(?, limit_tokens),
                model_allow = COALESCE(?, model_allow),
                model_deny = COALESCE(?, model_deny),
                rate_rpm = COALESCE(?, rate_rpm),
                max_concurrent = COALESCE(?, max_concurrent),
                window_start = COALESCE(?, window_start),
                window_end = COALESCE(?, window_end),
                updated_at = ?
               WHERE id = ?"#,
        )
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
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!("budget rule {id} not found");
        }

        sqlx::query_as::<_, BudgetRule>(
            "SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens, \
             model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at, \
             project, window_start, window_end \
             FROM budget_rules WHERE id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{BudgetScope, NewBudgetRule};
    use crate::db::repositories::budgets::BudgetRepository;
    use crate::db::sqlite::SqliteDb;

    async fn make_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    fn global_monthly_rule() -> NewBudgetRule {
        NewBudgetRule {
            user_id: None,
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: Some(100.0),
            limit_tokens: None,
            model_allow: vec![],
            model_deny: vec![],
            rate_rpm: None,
            max_concurrent: None,
            window_start: None,
            window_end: None,
        }
    }

    #[tokio::test]
    async fn list_for_scope_global_returns_global_rules() {
        let db = make_db().await;
        let rule = db.create(global_monthly_rule()).await.unwrap();
        let results = db.list_for_scope(&BudgetScope::Global).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, rule.id);
        assert!(results[0].user_id.is_none());
        assert!(results[0].group_name.is_none());
        assert!(results[0].project.is_none());
    }

    #[tokio::test]
    async fn list_for_scope_global_excludes_user_rules() {
        use crate::db::models::NewUser;
        use crate::db::repositories::users::UserRepository;
        let db = make_db().await;
        let user = UserRepository::create(&db, NewUser { name: "test".to_string(), email: None }).await.unwrap();
        let mut user_rule = global_monthly_rule();
        user_rule.user_id = Some(user.id);
        BudgetRepository::create(&db, user_rule).await.unwrap();
        let results = db.list_for_scope(&BudgetScope::Global).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn update_changes_limit_usd() {
        let db = make_db().await;
        let rule = db.create(global_monthly_rule()).await.unwrap();
        let updated = db.update(rule.id, &UpdateBudgetRule {
            limit_usd: Some(200.0),
            limit_tokens: None,
            model_allow: None,
            model_deny: None,
            rate_rpm: None,
            max_concurrent: None,
            window_start: None,
            window_end: None,
        }).await.unwrap();
        assert_eq!(updated.limit_usd, Some(200.0));
        assert_eq!(updated.window, "monthly"); // window type unchanged
    }

    #[tokio::test]
    async fn create_with_project_scope() {
        let db = make_db().await;
        let rule = db.create(NewBudgetRule {
            user_id: None,
            group_name: None,
            api_key_id: None,
            tag: None,
            project: Some("billing".to_string()),
            window: "monthly".to_string(),
            limit_usd: Some(50.0),
            limit_tokens: None,
            model_allow: vec![],
            model_deny: vec![],
            rate_rpm: None,
            max_concurrent: None,
            window_start: None,
            window_end: None,
        }).await.unwrap();
        assert_eq!(rule.project, Some("billing".to_string()));
        let results = db.list_for_scope(&BudgetScope::Project("billing".to_string())).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, rule.id);
        let global = db.list_for_scope(&BudgetScope::Global).await.unwrap();
        assert!(global.is_empty());
    }

    #[tokio::test]
    async fn create_group_target_rule() {
        let db = make_db().await;
        let rule = db.create(NewBudgetRule {
            user_id: None,
            group_name: Some("engineering".to_string()),
            api_key_id: None,
            tag: None,
            project: None,
            window: "target".to_string(),
            limit_usd: Some(500.0),
            limit_tokens: None,
            model_allow: vec![],
            model_deny: vec![],
            rate_rpm: None,
            max_concurrent: None,
            window_start: None,
            window_end: None,
        }).await.unwrap();
        let results = db.list_for_scope(&BudgetScope::Group("engineering".to_string())).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, rule.id);
    }
}
