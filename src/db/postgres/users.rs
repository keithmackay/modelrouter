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
    email: Option<String>,
    enabled: bool,
    created_at: String,
    metadata: String,
    spend_reset_at: Option<String>,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            name: r.name,
            email: r.email,
            enabled: r.enabled,
            created_at: r.created_at,
            metadata: r.metadata,
            api_key_id: None,
            spend_reset_at: r.spend_reset_at,
            api_key_project: None,
        }
    }
}

#[async_trait]
impl UserRepository for PostgresDb {
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<User>> {
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, email, enabled, created_at, metadata, spend_reset_at
               FROM users WHERE name = $1"#,
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(User::from))
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<User>> {
        let row = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, email, enabled, created_at, metadata, spend_reset_at
               FROM users WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(User::from))
    }

    async fn list(&self) -> anyhow::Result<Vec<User>> {
        let rows = sqlx::query_as::<_, UserRow>(
            r#"SELECT id, name, email, enabled, created_at, metadata, spend_reset_at
               FROM users ORDER BY enabled DESC, created_at DESC"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    async fn create(&self, user: NewUser) -> anyhow::Result<User> {
        let now = now_utc();
        let row = sqlx::query_as::<_, UserRow>(
            r#"INSERT INTO users (name, email, enabled, created_at, metadata)
               VALUES ($1, $2, true, $3, '{}')
               RETURNING id, name, email, enabled, created_at, metadata, spend_reset_at"#,
        )
        .bind(&user.name)
        .bind(&user.email)
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

    async fn reset_spend(&self, user_id: i64) -> anyhow::Result<()> {
        let now = now_utc();
        sqlx::query("UPDATE users SET spend_reset_at = $1 WHERE id = $2")
            .bind(&now)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
