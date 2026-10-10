use async_trait::async_trait;

use crate::db::models::LearnedModelCapability;
use crate::db::repositories::learned_capabilities::LearnedCapabilityRepository;
use super::SqliteDb;

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
impl LearnedCapabilityRepository for SqliteDb {
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
            "INSERT INTO learned_model_capabilities (model, supports_temperature, error, learned_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(model) DO UPDATE SET supports_temperature = excluded.supports_temperature, \
             error = excluded.error, learned_at = excluded.learned_at",
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
        let result = sqlx::query("DELETE FROM learned_model_capabilities WHERE model = ?")
            .bind(model)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    fn row(model: &str, error: &str) -> LearnedModelCapability {
        LearnedModelCapability {
            model: model.to_string(),
            supports_temperature: false,
            error: error.to_string(),
            learned_at: "2026-10-02T17:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn upsert_list_and_delete() {
        let db = test_db().await;
        db.upsert_learned_capability(&row("model-x@v1", "first")).await.unwrap();
        db.upsert_learned_capability(&row("model-x@v1", "second")).await.unwrap();
        db.upsert_learned_capability(&row("model-a", "other")).await.unwrap();

        let list = db.list_learned_capabilities().await.unwrap();
        assert_eq!(list.len(), 2, "upsert must not duplicate a model");
        assert_eq!(list[0].model, "model-a");
        assert_eq!(list[1].error, "second");
        assert!(!list[1].supports_temperature);

        assert!(db.delete_learned_capability("model-x@v1").await.unwrap());
        assert!(!db.delete_learned_capability("model-x@v1").await.unwrap());
        assert_eq!(db.list_learned_capabilities().await.unwrap().len(), 1);
    }
}
