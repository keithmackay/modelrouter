use async_trait::async_trait;

use crate::db::repositories::maintenance::{MaintenanceRepository, ReclaimOutcome};
use super::SqliteDb;

/// `PRAGMA auto_vacuum` values.
const AUTO_VACUUM_NONE: i64 = 0;
const AUTO_VACUUM_FULL: i64 = 1;

impl SqliteDb {
    /// The file's `auto_vacuum` mode (0 none, 1 full, 2 incremental), read
    /// fresh on one pooled connection.
    pub async fn auto_vacuum_mode(&self) -> anyhow::Result<i64> {
        let mut conn = self.pool.acquire().await?;
        Self::auto_vacuum_mode_on(&mut conn).await
    }

    /// `PRAGMA auto_vacuum` answers from the header the connection last read
    /// and opens no read transaction, so an idle pooled connection that saw
    /// the file before a `VACUUM` on another connection reports the old
    /// mode. A schema read first makes it re-read page 1 when the file has
    /// changed, and the pragma then tells the truth.
    async fn auto_vacuum_mode_on(conn: &mut sqlx::SqliteConnection) -> anyhow::Result<i64> {
        sqlx::query("SELECT count(*) FROM sqlite_master").execute(&mut *conn).await?;
        let (mode,): (i64,) = sqlx::query_as("PRAGMA auto_vacuum").fetch_one(&mut *conn).await?;
        Ok(mode)
    }

    async fn freelist_pages_on(conn: &mut sqlx::SqliteConnection) -> anyhow::Result<u64> {
        let (n,): (i64,) = sqlx::query_as("PRAGMA freelist_count").fetch_one(&mut *conn).await?;
        Ok(n.max(0) as u64)
    }

    /// Pages on the freelist: space deleted rows left behind that the file
    /// still occupies.
    pub async fn freelist_pages(&self) -> anyhow::Result<u64> {
        let (n,): (i64,) = sqlx::query_as("PRAGMA freelist_count").fetch_one(&self.pool).await?;
        Ok(n.max(0) as u64)
    }

    /// Pages in the file, so a caller can report its size (`page_count *
    /// page_size`) without touching the file system.
    pub async fn page_count_and_size(&self) -> anyhow::Result<(u64, u64)> {
        let (pages,): (i64,) = sqlx::query_as("PRAGMA page_count").fetch_one(&self.pool).await?;
        let (size,): (i64,) = sqlx::query_as("PRAGMA page_size").fetch_one(&self.pool).await?;
        Ok((pages.max(0) as u64, size.max(0) as u64))
    }

    /// Convert the file to incremental auto-vacuum and compact it: one full
    /// `VACUUM`, which rewrites the file (it needs free disk of about the
    /// file's size and holds the write lock while it runs). An operator
    /// step for a quiet window, never the hourly tick's job; after it, the
    /// tick's `reclaim_space` keeps the file trimmed. Returns the freelist
    /// pages reclaimed. A file already in incremental mode is still
    /// compacted, so the command doubles as "shrink it now".
    pub async fn convert_to_incremental_vacuum(&self) -> anyhow::Result<u64> {
        let before = self.freelist_pages().await?;
        // auto_vacuum only changes mode when a VACUUM follows it on the same
        // connection, so both statements go down one connection.
        let mut conn = self.pool.acquire().await?;
        sqlx::query("PRAGMA auto_vacuum = INCREMENTAL").execute(&mut *conn).await?;
        sqlx::query("VACUUM").execute(&mut *conn).await?;
        drop(conn);
        let after = self.freelist_pages().await?;
        Ok(before.saturating_sub(after))
    }
}

#[async_trait]
impl MaintenanceRepository for SqliteDb {
    async fn reclaim_space(&self) -> anyhow::Result<ReclaimOutcome> {
        // One connection for the whole pass, so the mode, the counts and
        // the vacuum all see the same file state.
        let mut conn = self.pool.acquire().await?;
        match Self::auto_vacuum_mode_on(&mut conn).await? {
            AUTO_VACUUM_NONE => {
                Ok(ReclaimOutcome::NotIncremental { free_pages: Self::freelist_pages_on(&mut conn).await? })
            }
            AUTO_VACUUM_FULL => Ok(ReclaimOutcome::ManagedByEngine),
            _ => {
                let before = Self::freelist_pages_on(&mut conn).await?;
                if before == 0 {
                    return Ok(ReclaimOutcome::IncrementalVacuum { freed_pages: 0 });
                }
                // No page count: release every free page. The statement
                // only touches the freelist, so it is cheap at any size.
                sqlx::query("PRAGMA incremental_vacuum").execute(&mut *conn).await?;
                let after = Self::freelist_pages_on(&mut conn).await?;
                Ok(ReclaimOutcome::IncrementalVacuum { freed_pages: before.saturating_sub(after) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn file_db(dir: &tempfile::TempDir, name: &str) -> SqliteDb {
        let path = dir.path().join(name);
        let db = SqliteDb::connect(path.to_str().unwrap()).await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    /// Enough rows to span many pages, then delete them all.
    async fn fill_and_empty(db: &SqliteDb) {
        let payload = "x".repeat(2000);
        for i in 0..200 {
            sqlx::query(
                "INSERT INTO audit_log (actor_name, action, target, created_at) VALUES ('op', 'test', ?, ?)",
            )
            .bind(&payload)
            .bind(format!("2026-01-01T00:00:{:02}Z", i % 60))
            .execute(&db.pool)
            .await
            .unwrap();
        }
        sqlx::query("DELETE FROM audit_log").execute(&db.pool).await.unwrap();
    }

    #[tokio::test]
    async fn a_new_file_is_created_in_incremental_mode_and_reclaim_shrinks_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = file_db(&dir, "fresh.db").await;
        assert_eq!(db.auto_vacuum_mode().await.unwrap(), 2, "new files start incremental");

        fill_and_empty(&db).await;
        let free = db.freelist_pages().await.unwrap();
        assert!(free > 0, "deleting rows leaves free pages");
        let (pages_before, _) = db.page_count_and_size().await.unwrap();

        let outcome = db.reclaim_space().await.unwrap();
        assert!(
            matches!(outcome, ReclaimOutcome::IncrementalVacuum { freed_pages } if freed_pages == free),
            "{outcome:?} (free was {free})"
        );
        assert_eq!(db.freelist_pages().await.unwrap(), 0);
        let (pages_after, _) = db.page_count_and_size().await.unwrap();
        assert!(pages_after < pages_before, "{pages_after} < {pages_before}");

        // Nothing left to free: the next pass is a no-op.
        assert_eq!(db.reclaim_space().await.unwrap(), ReclaimOutcome::IncrementalVacuum { freed_pages: 0 });
    }

    #[tokio::test]
    async fn a_legacy_file_reports_its_free_pages_until_converted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        // A file made before this change: auto_vacuum off at creation.
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::None);
            let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
            sqlx::migrate!("./migrations").run(&pool).await.unwrap();
            pool.close().await;
        }
        let db = SqliteDb::connect(path.to_str().unwrap()).await.unwrap();
        assert_eq!(db.auto_vacuum_mode().await.unwrap(), 0, "connecting does not convert an existing file");

        fill_and_empty(&db).await;
        let free = db.freelist_pages().await.unwrap();
        assert!(free > 0);
        assert_eq!(db.reclaim_space().await.unwrap(), ReclaimOutcome::NotIncremental { free_pages: free });
        assert_eq!(db.freelist_pages().await.unwrap(), free, "the tick never rewrites a legacy file");

        // The operator's one-time conversion compacts it and switches the mode.
        let (pages_before, _) = db.page_count_and_size().await.unwrap();
        let reclaimed = db.convert_to_incremental_vacuum().await.unwrap();
        assert_eq!(reclaimed, free);
        assert_eq!(db.auto_vacuum_mode().await.unwrap(), 2);
        assert_eq!(db.freelist_pages().await.unwrap(), 0);
        let (pages_after, _) = db.page_count_and_size().await.unwrap();
        assert!(pages_after < pages_before, "{pages_after} < {pages_before}");

        // From here the hourly pass keeps it trimmed on its own.
        fill_and_empty(&db).await;
        assert!(matches!(
            db.reclaim_space().await.unwrap(),
            ReclaimOutcome::IncrementalVacuum { freed_pages } if freed_pages > 0
        ));
    }

    #[tokio::test]
    async fn a_connection_idle_across_the_conversion_reads_the_new_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::None);
            let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
            sqlx::migrate!("./migrations").run(&pool).await.unwrap();
            pool.close().await;
        }
        let db = SqliteDb::connect(path.to_str().unwrap()).await.unwrap();
        // An idle connection that read the header before the VACUUM, held
        // so the conversion has to run on another one. This is the pool
        // state the full test run produced once (the pragma answered 0).
        let mut idle = db.pool.acquire().await.unwrap();
        assert_eq!(SqliteDb::auto_vacuum_mode_on(&mut idle).await.unwrap(), 0);
        db.convert_to_incremental_vacuum().await.unwrap();
        assert_eq!(SqliteDb::auto_vacuum_mode_on(&mut idle).await.unwrap(), 2, "the fresh read sees the conversion");
        drop(idle);
        assert_eq!(db.auto_vacuum_mode().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn an_in_memory_database_is_left_alone() {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        // Whatever mode the engine picks for memory, reclaim never fails.
        db.reclaim_space().await.unwrap();
    }
}
