#![cfg(feature = "postgres")]

use async_trait::async_trait;
use crate::db::models::{ApiKey, NewApiKey};
use crate::db::repositories::api_keys::ApiKeyRepository;
use super::{PostgresDb, now_utc};

/// Intermediate row type for Postgres BOOLEAN → bool mapping
#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: i64,
    user_id: i64,
    key_hash: String,
    label: Option<String>,
    enabled: bool,
    created_at: String,
    expires_at: Option<String>,
    project: Option<String>,
}

impl From<ApiKeyRow> for ApiKey {
    fn from(r: ApiKeyRow) -> Self {
        ApiKey {
            id: r.id,
            user_id: r.user_id,
            key_hash: r.key_hash,
            label: r.label,
            enabled: r.enabled,
            created_at: r.created_at,
            expires_at: r.expires_at,
            project: r.project,
        }
    }
}

#[async_trait]
impl ApiKeyRepository for PostgresDb {
    async fn find_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at, project FROM api_keys WHERE key_hash = $1 AND enabled = true"
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ApiKey::from))
    }

    async fn list_api_keys_for_user(&self, user_id: i64) -> anyhow::Result<Vec<ApiKey>> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at, project FROM api_keys WHERE user_id = $1 ORDER BY id"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ApiKey::from).collect())
    }

    async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey> {
        let now = now_utc();
        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"INSERT INTO api_keys (user_id, key_hash, label, enabled, created_at, expires_at, project)
               VALUES ($1, $2, $3, true, $4, $5, $6)
               RETURNING id, user_id, key_hash, label, enabled, created_at, expires_at, project"#
        )
        .bind(key.user_id)
        .bind(&key.key_hash)
        .bind(&key.label)
        .bind(&now)
        .bind(&key.expires_at)
        .bind(&key.project)
        .fetch_one(&self.pool)
        .await?;
        Ok(ApiKey::from(row))
    }

    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE api_keys SET enabled = false WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_all_api_keys(&self) -> anyhow::Result<Vec<ApiKey>> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at, project FROM api_keys ORDER BY enabled DESC, created_at DESC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ApiKey::from).collect())
    }

    async fn set_key_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE api_keys SET enabled = $1 WHERE id = $2")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn disable_all_keys_for_user(&self, user_id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE api_keys SET enabled = FALSE WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn find_key_by_user_project(&self, user_id: i64, project: Option<&str>) -> anyhow::Result<Option<ApiKey>> {
        let row = match project {
            Some(p) => sqlx::query_as::<_, ApiKeyRow>(
                "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at, project FROM api_keys WHERE user_id = $1 AND project = $2 LIMIT 1"
            ).bind(user_id).bind(p).fetch_optional(&self.pool).await?,
            None => sqlx::query_as::<_, ApiKeyRow>(
                "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at, project FROM api_keys WHERE user_id = $1 AND project IS NULL LIMIT 1"
            ).bind(user_id).fetch_optional(&self.pool).await?,
        };
        Ok(row.map(ApiKey::from))
    }
}
