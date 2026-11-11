#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::repositories::maintenance::{MaintenanceRepository, ReclaimOutcome};
use super::PostgresDb;

#[async_trait]
impl MaintenanceRepository for PostgresDb {
    /// Postgres autovacuum reclaims dead tuples on its own schedule; the
    /// purge's `DELETE` is all the router has to do.
    async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome> {
        Ok(ReclaimOutcome::ManagedByEngine)
    }
}
