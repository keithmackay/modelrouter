use async_trait::async_trait;
use crate::db::models::{ApiKey, NewApiKey};
use crate::db::repositories::api_keys::ApiKeyRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: i64,
    user_id: i64,
    key_hash: String,
    label: Option<String>,
    enabled: i64,
    created_at: String,
    #[sqlx(default)]
    expires_at: Option<String>,
}

impl From<ApiKeyRow> for ApiKey {
    fn from(r: ApiKeyRow) -> Self {
        ApiKey {
            id: r.id,
            user_id: r.user_id,
            key_hash: r.key_hash,
            label: r.label,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            expires_at: r.expires_at,
        }
    }
}

#[async_trait]
impl ApiKeyRepository for SqliteDb {
    async fn find_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at FROM api_keys WHERE key_hash = ? AND enabled = 1"
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ApiKey::from))
    }

    async fn list_api_keys_for_user(&self, user_id: i64) -> anyhow::Result<Vec<ApiKey>> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at FROM api_keys WHERE user_id = ? ORDER BY id"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ApiKey::from).collect())
    }

    async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey> {
        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO api_keys (user_id, key_hash, label, enabled, created_at, expires_at) VALUES (?, ?, ?, 1, ?, ?)"
        )
        .bind(key.user_id)
        .bind(&key.key_hash)
        .bind(&key.label)
        .bind(&now)
        .bind(&key.expires_at)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at FROM api_keys WHERE id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(ApiKey::from(row))
    }

    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
