#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{ModelAlias, NewModelAlias, NewScopedAlias, ScopedAlias};
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

#[derive(sqlx::FromRow)]
struct ScopedAliasRow {
    tag_key: String,
    tag_value: String,
    alias: String,
    target: String,
    provider: String,
    model: String,
    expires_at: i64,
    created_by: Option<String>,
    created_at: String,
}

impl From<ScopedAliasRow> for ScopedAlias {
    fn from(r: ScopedAliasRow) -> Self {
        ScopedAlias {
            tag_key: r.tag_key,
            tag_value: r.tag_value,
            alias: r.alias,
            target: r.target,
            provider: r.provider,
            model: r.model,
            expires_at: r.expires_at,
            created_by: r.created_by,
            created_at: r.created_at,
        }
    }
}

const SCOPED_COLS: &str =
    "tag_key, tag_value, alias, target, provider, model, expires_at, created_by, created_at";

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

    async fn list_scoped_aliases(&self) -> anyhow::Result<Vec<ScopedAlias>> {
        let rows = sqlx::query_as::<_, ScopedAliasRow>(&format!(
            "SELECT {SCOPED_COLS} FROM scoped_aliases ORDER BY tag_key, tag_value, alias"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ScopedAlias::from).collect())
    }

    async fn replace_scoped_aliases(
        &self,
        tag_key: &str,
        tag_value: &str,
        aliases: &[NewScopedAlias],
        expires_at: i64,
        created_by: Option<&str>,
    ) -> anyhow::Result<Vec<ScopedAlias>> {
        let now = now_utc();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM scoped_aliases WHERE tag_key = $1 AND tag_value = $2")
            .bind(tag_key)
            .bind(tag_value)
            .execute(&mut *tx)
            .await?;
        for entry in aliases {
            sqlx::query(
                "INSERT INTO scoped_aliases \
                 (tag_key, tag_value, alias, target, provider, model, expires_at, created_by, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(tag_key)
            .bind(tag_value)
            .bind(&entry.alias)
            .bind(&entry.target)
            .bind(&entry.provider)
            .bind(&entry.model)
            .bind(expires_at)
            .bind(created_by)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        let rows = sqlx::query_as::<_, ScopedAliasRow>(&format!(
            "SELECT {SCOPED_COLS} FROM scoped_aliases WHERE tag_key = $1 AND tag_value = $2 ORDER BY alias"
        ))
        .bind(tag_key)
        .bind(tag_value)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(ScopedAlias::from).collect())
    }

    async fn delete_expired_scoped_aliases(&self, now_epoch: i64) -> anyhow::Result<Vec<ScopedAlias>> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_as::<_, ScopedAliasRow>(&format!(
            "SELECT {SCOPED_COLS} FROM scoped_aliases WHERE expires_at != 0 AND expires_at <= $1 \
             ORDER BY tag_key, tag_value, alias"
        ))
        .bind(now_epoch)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM scoped_aliases WHERE expires_at != 0 AND expires_at <= $1")
            .bind(now_epoch)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(ScopedAlias::from).collect())
    }
}
