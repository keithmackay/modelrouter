#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::models::{AdminUser, NewAdminUser};
use crate::db::repositories::admin_users::AdminUserRepository;
use super::{PostgresDb, now_utc};

#[derive(sqlx::FromRow)]
struct AdminUserRow {
    id: i64,
    name: String,
    password_hash: String,
    role: String,
    enabled: bool,
    created_at: String,
    last_login_at: Option<String>,
}

impl From<AdminUserRow> for AdminUser {
    fn from(r: AdminUserRow) -> Self {
        AdminUser {
            id: r.id,
            name: r.name,
            password_hash: r.password_hash,
            role: r.role,
            enabled: r.enabled,
            created_at: r.created_at,
            last_login_at: r.last_login_at,
        }
    }
}

#[async_trait]
impl AdminUserRepository for PostgresDb {
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<AdminUser>> {
        let row = sqlx::query_as::<_, AdminUserRow>(
            "SELECT id, name, password_hash, role, enabled, created_at, last_login_at
             FROM admin_users WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(AdminUser::from))
    }

    async fn find_by_id(&self, id: i64) -> anyhow::Result<Option<AdminUser>> {
        let row = sqlx::query_as::<_, AdminUserRow>(
            "SELECT id, name, password_hash, role, enabled, created_at, last_login_at
             FROM admin_users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(AdminUser::from))
    }

    async fn list(&self) -> anyhow::Result<Vec<AdminUser>> {
        let rows = sqlx::query_as::<_, AdminUserRow>(
            "SELECT id, name, password_hash, role, enabled, created_at, last_login_at
             FROM admin_users ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(AdminUser::from).collect())
    }

    async fn create(&self, user: NewAdminUser) -> anyhow::Result<AdminUser> {
        let now = now_utc();
        let row = sqlx::query_as::<_, AdminUserRow>(
            r#"INSERT INTO admin_users (name, password_hash, role, enabled, created_at)
               VALUES ($1, $2, $3, true, $4)
               RETURNING id, name, password_hash, role, enabled, created_at, last_login_at"#,
        )
        .bind(&user.name)
        .bind(&user.password_hash)
        .bind(&user.role)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        Ok(AdminUser::from(row))
    }

    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE admin_users SET enabled = $1 WHERE id = $2")
            .bind(enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM admin_users WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_last_login(&self, id: i64) -> anyhow::Result<()> {
        let now = now_utc();
        sqlx::query("UPDATE admin_users SET last_login_at = $1 WHERE id = $2")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
