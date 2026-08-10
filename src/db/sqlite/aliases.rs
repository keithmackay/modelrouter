use async_trait::async_trait;

use crate::db::models::{ModelAlias, NewModelAlias};
use crate::db::repositories::aliases::AliasRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct AliasRow {
    alias: String,
    target: String,
    created_by: Option<String>,
    created_at: String,
    updated_at: String,
}

impl From<AliasRow> for ModelAlias {
    fn from(r: AliasRow) -> Self {
        ModelAlias {
            alias: r.alias,
            target: r.target,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

const SELECT_COLS: &str = "alias, target, created_by, created_at, updated_at";

#[async_trait]
impl AliasRepository for SqliteDb {
    async fn list_aliases(&self) -> anyhow::Result<Vec<ModelAlias>> {
        let rows = sqlx::query_as::<_, AliasRow>(&format!(
            "SELECT {SELECT_COLS} FROM model_aliases ORDER BY alias"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ModelAlias::from).collect())
    }

    async fn get_alias(&self, alias: &str) -> anyhow::Result<Option<ModelAlias>> {
        let row = sqlx::query_as::<_, AliasRow>(&format!(
            "SELECT {SELECT_COLS} FROM model_aliases WHERE alias = ?"
        ))
        .bind(alias)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ModelAlias::from))
    }

    async fn upsert_alias(&self, new: NewModelAlias) -> anyhow::Result<ModelAlias> {
        let now = now_utc();
        sqlx::query(
            "INSERT INTO model_aliases (alias, target, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(alias) DO UPDATE SET target = excluded.target, updated_at = excluded.updated_at",
        )
        .bind(&new.alias)
        .bind(&new.target)
        .bind(&new.created_by)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let row = sqlx::query_as::<_, AliasRow>(&format!(
            "SELECT {SELECT_COLS} FROM model_aliases WHERE alias = ?"
        ))
        .bind(&new.alias)
        .fetch_one(&self.pool)
        .await?;
        Ok(ModelAlias::from(row))
    }

    async fn delete_alias(&self, alias: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM model_aliases WHERE alias = ?")
            .bind(alias)
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

    fn new_alias(alias: &str, target: &str) -> NewModelAlias {
        NewModelAlias {
            alias: alias.to_string(),
            target: target.to_string(),
            created_by: Some("tester".to_string()),
        }
    }

    #[tokio::test]
    async fn upsert_creates_then_replaces_target() {
        let db = test_db().await;
        let created = db.upsert_alias(new_alias("deep", "anthropic/claude-opus-4-6")).await.unwrap();
        assert_eq!(created.target, "anthropic/claude-opus-4-6");
        assert_eq!(created.created_by.as_deref(), Some("tester"));

        let updated = db.upsert_alias(new_alias("deep", "openai/gpt-5")).await.unwrap();
        assert_eq!(updated.target, "openai/gpt-5");

        // Upsert must not create a duplicate row.
        assert_eq!(db.list_aliases().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn get_and_delete_alias() {
        let db = test_db().await;
        db.upsert_alias(new_alias("fast", "openai/gpt-5-mini")).await.unwrap();
        assert!(db.get_alias("fast").await.unwrap().is_some());
        assert!(db.get_alias("missing").await.unwrap().is_none());

        assert!(db.delete_alias("fast").await.unwrap());
        assert!(!db.delete_alias("fast").await.unwrap());
        assert!(db.get_alias("fast").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_is_sorted_by_alias() {
        let db = test_db().await;
        db.upsert_alias(new_alias("zeta", "a/b")).await.unwrap();
        db.upsert_alias(new_alias("alpha", "c/d")).await.unwrap();
        let list = db.list_aliases().await.unwrap();
        assert_eq!(list[0].alias, "alpha");
        assert_eq!(list[1].alias, "zeta");
    }
}
