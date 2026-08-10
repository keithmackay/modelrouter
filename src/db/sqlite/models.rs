use async_trait::async_trait;
use crate::db::models::{Model, ModelFailover, NewModel, ProviderState};
use crate::db::repositories::models::ModelRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: i64,
    provider: String,
    name: String,
    alias: Option<String>,
    enabled: i64,
    created_at: String,
    disabled_reason: Option<String>,
    disabled_by: Option<String>,
    disabled_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ProviderStateRow {
    provider: String,
    enabled: i64,
    disabled_reason: Option<String>,
    disabled_by: Option<String>,
    disabled_at: Option<String>,
    updated_at: String,
}

impl From<ProviderStateRow> for ProviderState {
    fn from(r: ProviderStateRow) -> Self {
        ProviderState {
            provider: r.provider,
            enabled: r.enabled != 0,
            disabled_reason: r.disabled_reason,
            disabled_by: r.disabled_by,
            disabled_at: r.disabled_at,
            updated_at: r.updated_at,
        }
    }
}

const MODEL_COLS: &str =
    "id, provider, name, alias, enabled, created_at, disabled_reason, disabled_by, disabled_at";
const PROVIDER_STATE_COLS: &str =
    "provider, enabled, disabled_reason, disabled_by, disabled_at, updated_at";

impl From<ModelRow> for Model {
    fn from(r: ModelRow) -> Self {
        Model {
            id: r.id,
            provider: r.provider,
            name: r.name,
            alias: r.alias,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            disabled_reason: r.disabled_reason,
            disabled_by: r.disabled_by,
            disabled_at: r.disabled_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct FailoverRow {
    id: i64,
    primary_model: String,
    fallback_model: String,
    priority: i64,
}

impl From<FailoverRow> for ModelFailover {
    fn from(r: FailoverRow) -> Self {
        ModelFailover {
            id: r.id,
            primary_model: r.primary_model,
            fallback_model: r.fallback_model,
            priority: r.priority,
        }
    }
}

#[async_trait]
impl ModelRepository for SqliteDb {
    async fn create_model(&self, model: NewModel) -> anyhow::Result<Model> {
        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO models (provider, name, alias, enabled, created_at) VALUES (?, ?, ?, 1, ?)"
        )
        .bind(&model.provider)
        .bind(&model.name)
        .bind(&model.alias)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, ModelRow>(
            &format!("SELECT {MODEL_COLS} FROM models WHERE id = ?")
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(Model::from(row))
    }

    async fn list_models(&self) -> anyhow::Result<Vec<Model>> {
        let rows = sqlx::query_as::<_, ModelRow>(
            &format!("SELECT {MODEL_COLS} FROM models ORDER BY provider, name")
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Model::from).collect())
    }

    async fn get_model(&self, id: i64) -> anyhow::Result<Option<Model>> {
        let row = sqlx::query_as::<_, ModelRow>(
            &format!("SELECT {MODEL_COLS} FROM models WHERE id = ?")
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Model::from))
    }

    async fn set_model_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        self.set_model_enabled_with_reason(id, enabled, None, None).await
    }

    async fn set_model_enabled_with_reason(
        &self,
        id: i64,
        enabled: bool,
        reason: Option<&str>,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
        if enabled {
            sqlx::query(
                "UPDATE models SET enabled = 1, disabled_reason = NULL, \
                 disabled_by = NULL, disabled_at = NULL WHERE id = ?",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE models SET enabled = 0, disabled_reason = ?, \
                 disabled_by = ?, disabled_at = ? WHERE id = ?",
            )
            .bind(reason)
            .bind(by)
            .bind(now_utc())
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn list_provider_states(&self) -> anyhow::Result<Vec<ProviderState>> {
        let rows = sqlx::query_as::<_, ProviderStateRow>(&format!(
            "SELECT {PROVIDER_STATE_COLS} FROM provider_states ORDER BY provider"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ProviderState::from).collect())
    }

    async fn get_provider_state(&self, provider: &str) -> anyhow::Result<Option<ProviderState>> {
        let row = sqlx::query_as::<_, ProviderStateRow>(&format!(
            "SELECT {PROVIDER_STATE_COLS} FROM provider_states WHERE provider = ?"
        ))
        .bind(provider)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ProviderState::from))
    }

    async fn set_provider_enabled(
        &self,
        provider: &str,
        enabled: bool,
        reason: Option<&str>,
        by: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = now_utc();
        let (reason, by, at) = if enabled {
            (None, None, None)
        } else {
            (reason, by, Some(now.as_str()))
        };
        sqlx::query(
            "INSERT INTO provider_states \
               (provider, enabled, disabled_reason, disabled_by, disabled_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(provider) DO UPDATE SET \
               enabled = excluded.enabled, \
               disabled_reason = excluded.disabled_reason, \
               disabled_by = excluded.disabled_by, \
               disabled_at = excluded.disabled_at, \
               updated_at = excluded.updated_at",
        )
        .bind(provider)
        .bind(enabled as i64)
        .bind(reason)
        .bind(by)
        .bind(at)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_model(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM models WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_failovers(&self, primary_model: &str, fallbacks: &[String]) -> anyhow::Result<()> {
        // Delete existing chain for this primary
        sqlx::query("DELETE FROM model_failovers WHERE primary_model = ?")
            .bind(primary_model)
            .execute(&self.pool)
            .await?;

        // Insert new chain in priority order
        for (i, fallback) in fallbacks.iter().enumerate() {
            sqlx::query(
                "INSERT INTO model_failovers (primary_model, fallback_model, priority) VALUES (?, ?, ?)"
            )
            .bind(primary_model)
            .bind(fallback)
            .bind(i as i64)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn list_failovers(&self, primary_model: &str) -> anyhow::Result<Vec<ModelFailover>> {
        let rows = sqlx::query_as::<_, FailoverRow>(
            "SELECT id, primary_model, fallback_model, priority FROM model_failovers \
             WHERE primary_model = ? ORDER BY priority ASC"
        )
        .bind(primary_model)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ModelFailover::from).collect())
    }

    async fn list_all_failovers(&self) -> anyhow::Result<Vec<ModelFailover>> {
        let rows = sqlx::query_as::<_, FailoverRow>(
            "SELECT id, primary_model, fallback_model, priority FROM model_failovers \
             ORDER BY primary_model, priority ASC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ModelFailover::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::models::ModelRepository;
    use crate::db::models::NewModel;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_create_and_list() {
        let db = test_db().await;
        let m = db.create_model(NewModel {
            provider: "anthropic".to_string(),
            name: "claude-opus-4-6".to_string(),
            alias: Some("opus".to_string()),
        }).await.unwrap();
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.alias, Some("opus".to_string()));
        assert!(m.enabled);

        let list = db.list_models().await.unwrap();
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_set_enabled() {
        let db = test_db().await;
        let m = db.create_model(NewModel {
            provider: "openai".to_string(),
            name: "gpt-4o".to_string(),
            alias: None,
        }).await.unwrap();
        db.set_model_enabled(m.id, false).await.unwrap();
        let fetched = db.get_model(m.id).await.unwrap().unwrap();
        assert!(!fetched.enabled);
    }

    #[tokio::test]
    async fn test_delete() {
        let db = test_db().await;
        let m = db.create_model(NewModel {
            provider: "openai".to_string(),
            name: "gpt-4o".to_string(),
            alias: None,
        }).await.unwrap();
        assert!(db.delete_model(m.id).await.unwrap());
        assert!(!db.delete_model(999).await.unwrap());
    }

    #[tokio::test]
    async fn test_failovers() {
        let db = test_db().await;
        db.set_failovers("opus", &["sonnet".to_string(), "haiku".to_string()]).await.unwrap();
        let chain = db.list_failovers("opus").await.unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].fallback_model, "sonnet");
        assert_eq!(chain[1].fallback_model, "haiku");
        assert_eq!(chain[0].priority, 0);
        assert_eq!(chain[1].priority, 1);

        // Replace chain
        db.set_failovers("opus", &["haiku".to_string()]).await.unwrap();
        let chain2 = db.list_failovers("opus").await.unwrap();
        assert_eq!(chain2.len(), 1);
        assert_eq!(chain2[0].fallback_model, "haiku");
    }

    #[tokio::test]
    async fn test_list_all_failovers() {
        let db = test_db().await;
        db.set_failovers("a", &["b".to_string()]).await.unwrap();
        db.set_failovers("c", &["d".to_string(), "e".to_string()]).await.unwrap();
        let all = db.list_all_failovers().await.unwrap();
        assert_eq!(all.len(), 3);
    }
}
