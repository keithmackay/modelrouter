use async_trait::async_trait;

/// What a space-reclaim pass did, for the retention log line and for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimOutcome {
    /// SQLite in incremental auto-vacuum mode: free pages were returned to the
    /// file system. `freed_pages` is the freelist delta the pass measured.
    IncrementalVacuum { freed_pages: u64 },
    /// SQLite with auto-vacuum off: deleted rows leave free pages that new
    /// rows reuse, so the file stops growing once retention is in effect,
    /// but it does not shrink until an operator converts it (`db vacuum`).
    NotIncremental { free_pages: u64 },
    /// The storage engine reclaims space itself (SQLite full auto-vacuum,
    /// Postgres autovacuum): nothing for the tick to do.
    ManagedByEngine,
}

/// Storage housekeeping that is not a table: reclaiming the space the
/// retention purges free. Each backend implements what its engine needs.
#[async_trait]
pub trait MaintenanceRepository: Send + Sync {
    /// Return the space freed by deleted rows to the file system where the
    /// engine does not do it on its own. Cheap enough to run after every
    /// purge; a no-op when there is nothing to reclaim.
    async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome>;

    /// Become the one process that runs the cluster-wide job `job` now.
    /// `None` means another replica holds it: skip this tick. The lock is
    /// held until the returned guard drops, and the store releases it by
    /// itself if the holder dies, so a crashed replica never wedges a job.
    ///
    /// The default grants it unconditionally: right for a store only one
    /// process writes (SQLite). PostgreSQL overrides it with an advisory lock.
    async fn try_job_lock(&self, job: &str) -> anyhow::Result<Option<JobLock>> {
        let _ = job;
        Ok(Some(JobLock::unshared()))
    }
}

/// Proof of holding a job lock. Dropping it releases the lock.
pub struct JobLock {
    _held: Option<Box<dyn std::any::Any + Send>>,
}

impl JobLock {
    /// A lock nothing else contends for.
    pub fn unshared() -> Self {
        Self { _held: None }
    }

    /// A lock whose lifetime is `held`'s: releasing it is dropping that value.
    pub fn holding(held: impl std::any::Any + Send) -> Self {
        Self { _held: Some(Box::new(held)) }
    }
}

impl std::fmt::Debug for JobLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobLock").field("shared", &self._held.is_some()).finish()
    }
}

/// The 64-bit advisory-lock key for a job: the first eight bytes of
/// SHA-256 over a namespaced name, so it is stable across builds and
/// unlikely to collide with another application's keys on the same server.
pub fn job_lock_key(job: &str) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("modelrouter:job:{job}").as_bytes());
    i64::from_be_bytes(digest[..8].try_into().expect("eight bytes"))
}

/// Run `body` only if this process wins `job`'s lock; `None` when another
/// replica holds it (or the lock could not be asked for, which is logged).
pub async fn with_job_lock<D, F, T>(db: &D, job: &str, body: F) -> Option<T>
where
    D: MaintenanceRepository + ?Sized,
    F: std::future::Future<Output = T>,
{
    match db.try_job_lock(job).await {
        Ok(Some(lock)) => {
            let out = body.await;
            drop(lock);
            Some(out)
        }
        Ok(None) => {
            tracing::debug!(job, "another replica holds the job lock; skipping this tick");
            None
        }
        Err(e) => {
            tracing::warn!(job, error = %e, "could not take the job lock; skipping this tick");
            None
        }
    }
}

#[cfg(test)]
mod job_lock_tests {
    use super::*;

    struct Unshared;
    #[async_trait]
    impl MaintenanceRepository for Unshared {
        async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome> {
            Ok(ReclaimOutcome::ManagedByEngine)
        }
    }

    struct Held;
    #[async_trait]
    impl MaintenanceRepository for Held {
        async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome> {
            Ok(ReclaimOutcome::ManagedByEngine)
        }
        async fn try_job_lock(&self, _job: &str) -> anyhow::Result<Option<JobLock>> {
            Ok(None)
        }
    }

    struct Broken;
    #[async_trait]
    impl MaintenanceRepository for Broken {
        async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome> {
            Ok(ReclaimOutcome::ManagedByEngine)
        }
        async fn try_job_lock(&self, _job: &str) -> anyhow::Result<Option<JobLock>> {
            anyhow::bail!("store unreachable")
        }
    }

    #[tokio::test]
    async fn the_default_lock_is_always_granted() {
        assert_eq!(with_job_lock(&Unshared, "retention", async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn a_held_or_unavailable_lock_skips_the_body() {
        let ran = std::sync::atomic::AtomicBool::new(false);
        let body = async { ran.store(true, std::sync::atomic::Ordering::SeqCst) };
        assert_eq!(with_job_lock(&Held, "retention", body).await, None);
        assert_eq!(with_job_lock(&Broken, "retention", async {}).await, None);
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn keys_are_stable_and_distinct_per_job() {
        assert_eq!(job_lock_key("retention"), job_lock_key("retention"));
        assert_ne!(job_lock_key("retention"), job_lock_key("s3-archival"));
    }

    #[test]
    fn dropping_a_holding_lock_drops_what_it_holds() {
        struct Flag(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lock = JobLock::holding(Flag(released.clone()));
        assert!(!released.load(std::sync::atomic::Ordering::SeqCst));
        drop(lock);
        assert!(released.load(std::sync::atomic::Ordering::SeqCst));
    }
}
