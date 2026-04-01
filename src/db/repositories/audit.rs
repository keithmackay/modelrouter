use async_trait::async_trait;
use crate::db::models::{AuditLogEntry, NewAuditLogEntry};

#[async_trait]
pub trait AuditRepository: Send + Sync {
    async fn create(&self, entry: NewAuditLogEntry) -> anyhow::Result<AuditLogEntry>;
    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<AuditLogEntry>>;
    async fn list_by_actor(&self, actor_name: &str, limit: i64) -> anyhow::Result<Vec<AuditLogEntry>>;
}
