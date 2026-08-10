#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{ModelAlias, NewModelAlias};
use crate::db::repositories::aliases::AliasRepository;
use super::{PostgresDb, now_utc};

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

#[async_trait]
impl AliasRepository for PostgresDb {
    async fn list_aliases(&self) -> anyhow::Result<Vec<ModelAlias>> {
        let rows = sqlx::query_as::<_, AliasRow>(
            "SELECT alias, target, created_by, created_at, updated_at \
             FROM model_aliases ORDER BY alias",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ModelAlias::from).collect())
    }

    async fn get_alias(&self, alias: &str) -> anyhow::Result<Option<ModelAlias>> {
        let row = sqlx::query_as::<_, AliasRow>(
            "SELECT alias, target, created_by, created_at, updated_at \
             FROM model_aliases WHERE alias = $1",
        )
        .bind(alias)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ModelAlias::from))
    }

    async fn upsert_alias(&self, new: NewModelAlias) -> anyhow::Result<ModelAlias> {
        let now = now_utc();
        let row = sqlx::query_as::<_, AliasRow>(
            r#"INSERT INTO model_aliases (alias, target, created_by, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $4)
               ON CONFLICT (alias) DO UPDATE SET target = EXCLUDED.target, updated_at = EXCLUDED.updated_at
               RETURNING alias, target, created_by, created_at, updated_at"#,
        )
        .bind(&new.alias)
        .bind(&new.target)
        .bind(&new.created_by)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(ModelAlias::from(row))
    }

    async fn delete_alias(&self, alias: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM model_aliases WHERE alias = $1")
            .bind(alias)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
