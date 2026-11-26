#![cfg(feature = "postgres")]

use async_trait::async_trait;

use crate::db::repositories::maintenance::{job_lock_key, JobLock, MaintenanceRepository, ReclaimOutcome};
use super::PostgresDb;

#[async_trait]
impl MaintenanceRepository for PostgresDb {
    /// Postgres autovacuum reclaims dead tuples on its own schedule; the
    /// purge's `DELETE` is all the router has to do.
    async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome> {
        Ok(ReclaimOutcome::ManagedByEngine)
    }

    /// A session-level `pg_try_advisory_lock` on a connection taken out of
    /// the pool for as long as the lock is held. Releasing is closing that
    /// connection, which the server also does for a holder that dies, so the
    /// lock can never leak into a pooled connection or outlive its replica.
    async fn try_job_lock(&self, job: &str) -> anyhow::Result<Option<JobLock>> {
        let mut conn = self.pool.acquire().await?;
        let (won,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
            .bind(job_lock_key(job))
            .fetch_one(&mut *conn)
            .await?;
        if !won {
            return Ok(None); // back to the pool: it holds nothing
        }
        Ok(Some(JobLock::holding(conn.detach())))
    }
}
