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
}
