#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::models::{Model, ModelFailover, NewModel};
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
}

impl From<ModelRow> for Model {
    fn from(r: ModelRow) -> Self {
        Model {
            id: r.id,
            provider: r.provider,
            name: r.name,
            alias: r.alias,
            enabled: r.enabled,
            created_at: r.created_at,
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
            r#"INSERT INTO models (provider, name, alias, enabled, created_at)
               VALUES ($1, $2, $3, true, $4)
               RETURNING id, provider, name, alias, enabled, created_at"#
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
            "SELECT id, provider, name, alias, enabled, created_at FROM models ORDER BY provider, name"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Model::from).collect())
    }

    async fn get_model(&self, id: i64) -> anyhow::Result<Option<Model>> {
        let row = sqlx::query_as::<_, ModelRow>(
            "SELECT id, provider, name, alias, enabled, created_at FROM models WHERE id = $1"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Model::from))
    }

    async fn set_model_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE models SET enabled = $1 WHERE id = $2")
            .bind(enabled)
            .bind(id)
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
