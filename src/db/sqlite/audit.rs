use async_trait::async_trait;

use crate::db::models::{AuditLogEntry, NewAuditLogEntry};
use crate::db::repositories::audit::AuditRepository;
use super::{SqliteDb, now_utc};

#[async_trait]
impl AuditRepository for SqliteDb {
    async fn create(&self, entry: NewAuditLogEntry) -> anyhow::Result<AuditLogEntry> {
        let now = now_utc();
        let result = sqlx::query(
            r#"INSERT INTO audit_log (actor_id, actor_name, action, target, before_json, after_json, created_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(entry.actor_id)
        .bind(&entry.actor_name)
        .bind(&entry.action)
        .bind(&entry.target)
        .bind(&entry.before_json)
        .bind(&entry.after_json)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, AuditLogEntry>(
            "SELECT id, actor_id, actor_name, action, target, before_json, after_json, created_at
             FROM audit_log WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn list(&self, limit: i64) -> anyhow::Result<Vec<AuditLogEntry>> {
        let rows = sqlx::query_as::<_, AuditLogEntry>(
            "SELECT id, actor_id, actor_name, action, target, before_json, after_json, created_at
             FROM audit_log ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_by_actor(&self, actor_name: &str, limit: i64) -> anyhow::Result<Vec<AuditLogEntry>> {
        let rows = sqlx::query_as::<_, AuditLogEntry>(
            "SELECT id, actor_id, actor_name, action, target, before_json, after_json, created_at
             FROM audit_log WHERE actor_name = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(actor_name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
