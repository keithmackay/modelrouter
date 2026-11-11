use async_trait::async_trait;
use crate::db::models::{AuditLogEntry, NewAuditLogEntry};

#[async_trait]
pub trait AuditRepository: Send + Sync {
    async fn create(&self, entry: NewAuditLogEntry) -> anyhow::Result<AuditLogEntry>;
    async fn list(&self, limit: i64, offset: i64) -> anyhow::Result<Vec<AuditLogEntry>>;
    async fn list_by_actor(&self, actor_name: &str, limit: i64) -> anyhow::Result<Vec<AuditLogEntry>>;
    /// Delete audit rows with `created_at` before `cutoff_rfc3339` (the
    /// retention tick). Returns the number deleted.
    async fn purge_audit_older_than(&self, cutoff_rfc3339: &str) -> anyhow::Result<u64>;
}
