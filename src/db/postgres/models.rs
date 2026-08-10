#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::models::{Model, ModelFailover, NewModel, ProviderState};
use crate::db::repositories::models::ModelRepository;
use super::{PostgresDb, now_utc};

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: i64,
    provider: String,
    name: String,
    alias: Option<String>,
    enabled: bool,
    created_at: String,
    disabled_reason: Option<String>,
    disabled_by: Option<String>,
    disabled_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ProviderStateRow {
    provider: String,
    enabled: bool,
    disabled_reason: Option<String>,
    disabled_by: Option<String>,
    disabled_at: Option<String>,
    updated_at: String,
}

impl From<ProviderStateRow> for ProviderState {
    fn from(r: ProviderStateRow) -> Self {
        ProviderState {
            provider: r.provider,
            enabled: r.enabled,
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
            enabled: r.enabled,
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
impl ModelRepository for PostgresDb {
    async fn create_model(&self, model: NewModel) -> anyhow::Result<Model> {
        let now = now_utc();
        let row = sqlx::query_as::<_, ModelRow>(
            &format!(
                "INSERT INTO models (provider, name, alias, enabled, created_at) \
                 VALUES ($1, $2, $3, true, $4) RETURNING {MODEL_COLS}"
            )
        )
        .bind(&model.provider)
        .bind(&model.name)
        .bind(&model.alias)
        .bind(&now)
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
            &format!("SELECT {MODEL_COLS} FROM models WHERE id = $1")
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
                "UPDATE models SET enabled = true, disabled_reason = NULL, \
                 disabled_by = NULL, disabled_at = NULL WHERE id = $1",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE models SET enabled = false, disabled_reason = $1, \
                 disabled_by = $2, disabled_at = $3 WHERE id = $4",
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
            "SELECT {PROVIDER_STATE_COLS} FROM provider_states WHERE provider = $1"
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
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (provider) DO UPDATE SET \
               enabled = EXCLUDED.enabled, \
               disabled_reason = EXCLUDED.disabled_reason, \
               disabled_by = EXCLUDED.disabled_by, \
               disabled_at = EXCLUDED.disabled_at, \
               updated_at = EXCLUDED.updated_at",
        )
        .bind(provider)
        .bind(enabled)
        .bind(reason)
        .bind(by)
        .bind(at)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_model(&self, id: i64) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM models WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_failovers(&self, primary_model: &str, fallbacks: &[String]) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM model_failovers WHERE primary_model = $1")
            .bind(primary_model)
            .execute(&self.pool)
            .await?;

        for (i, fallback) in fallbacks.iter().enumerate() {
            sqlx::query(
                "INSERT INTO model_failovers (primary_model, fallback_model, priority) VALUES ($1, $2, $3)"
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
             WHERE primary_model = $1 ORDER BY priority ASC"
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
