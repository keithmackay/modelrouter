#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{NewUser, User};
use crate::db::repositories::users::UserRepository;
use super::{PostgresDb, now_utc};

/// Intermediate row type to handle Postgres BOOLEAN → bool conversion
#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    name: String,
    api_key: String,
    api_key_old: Option<String>,
    api_key_old_expires_at: Option<String>,
    group_name: Option<String>,
    enabled: bool,
    created_at: String,
    metadata: String,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            name: r.name,
            api_key: r.api_key,
            api_key_old: r.api_key_old,
            api_key_old_expires_at: r.api_key_old_expires_at,
            group_name: r.group_name,
            enabled: r.enabled,
            created_at: r.created_at,
            metadata: r.metadata,
        }
    }
}

#[async_trait]
impl UserRepository for PostgresDb {
    async fn find_by_api_key(&self, key_hash: &str) -> anyhow::Result<Option<User>> {
        let now = chrono::Utc::now().to_rfc3339();
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, api_key, api_key_old, api_key_old_expires_at,
                      group_name, enabled, created_at, metadata
               FROM users
               WHERE api_key = $1
                  OR (api_key_old = $2 AND api_key_old_expires_at > $3)
               LIMIT 1"#,
        )
        .bind(key_hash)
        .bind(key_hash)
        .bind(&now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(User::from))
    }

    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<User>> {
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, api_key, api_key_old, api_key_old_expires_at,
                      group_name, enabled, created_at, metadata
               FROM users WHERE name = $1"#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(User::from))
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<User>> {
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, api_key, api_key_old, api_key_old_expires_at,
                      group_name, enabled, created_at, metadata
               FROM users WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(User::from))
    }

    async fn list(&self) -> anyhow::Result<Vec<User>> {
        let rows = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, api_key, api_key_old, api_key_old_expires_at,
                      group_name, enabled, created_at, metadata
               FROM users ORDER BY id"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    async fn create(&self, user: NewUser) -> anyhow::Result<User> {
        let now = now_utc();
        let row = sqlx::query_as::<_, UserRow>(
            r#"INSERT INTO users (name, api_key, group_name, enabled, created_at, metadata)
               VALUES ($1, $2, $3, true, $4, '{}')
               RETURNING id, name, api_key, api_key_old, api_key_old_expires_at,
                         group_name, enabled, created_at, metadata"#,
        )
        .bind(&user.name)
        .bind(&user.api_key_hash)
        .bind(&user.group_name)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(User::from(row))
    }

    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE users SET enabled = $1 WHERE id = $2")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn rotate_key(&self, id: i64, new_key_hash: &str, overlap_expires_at: &str) -> anyhow::Result<()> {
        sqlx::query(
            r#"UPDATE users
               SET api_key_old = api_key,
                   api_key_old_expires_at = $1,
                   api_key = $2
               WHERE id = $3"#,
        )
        .bind(overlap_expires_at)
        .bind(new_key_hash)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn expire_old_keys(&self) -> anyhow::Result<u64> {
        let now = chrono::Utc::now().to_rfc3339();
        let result = sqlx::query(
            r#"UPDATE users
               SET api_key_old = NULL, api_key_old_expires_at = NULL
               WHERE api_key_old IS NOT NULL
                 AND api_key_old_expires_at <= $1"#,
        )
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
