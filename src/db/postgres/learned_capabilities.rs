#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::LearnedModelCapability;
use crate::db::repositories::learned_capabilities::LearnedCapabilityRepository;
use super::PostgresDb;

#[derive(sqlx::FromRow)]
struct Row {
    model: String,
    supports_temperature: bool,
    error: String,
    learned_at: String,
}

impl From<Row> for LearnedModelCapability {
    fn from(r: Row) -> Self {
        LearnedModelCapability {
            model: r.model,
            supports_temperature: r.supports_temperature,
            error: r.error,
            learned_at: r.learned_at,
        }
    }
}

#[async_trait]
impl LearnedCapabilityRepository for PostgresDb {
    async fn list_learned_capabilities(&self) -> anyhow::Result<Vec<LearnedModelCapability>> {
        let rows = sqlx::query_as::<_, Row>(
            "SELECT model, supports_temperature, error, learned_at \
             FROM learned_model_capabilities ORDER BY model",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(LearnedModelCapability::from).collect())
    }

    async fn upsert_learned_capability(&self, row: &LearnedModelCapability) -> anyhow::Result<()> {
        sqlx::query(
            r#"INSERT INTO learned_model_capabilities (model, supports_temperature, error, learned_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (model) DO UPDATE SET supports_temperature = EXCLUDED.supports_temperature,
               error = EXCLUDED.error, learned_at = EXCLUDED.learned_at"#,
        )
        .bind(&row.model)
        .bind(row.supports_temperature)
        .bind(&row.error)
        .bind(&row.learned_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_learned_capability(&self, model: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM learned_model_capabilities WHERE model = $1")
            .bind(model)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
