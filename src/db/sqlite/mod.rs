use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::str::FromStr;

mod aliases;
mod learned_capabilities;
mod groups;
mod users;
mod admin_users;
mod api_keys;
mod sessions;
mod app_settings;
mod prompts;
mod costs;
mod experiments;
mod failures;
mod budgets;
mod audit;
mod hooks;
mod maintenance;
mod rate_limits;
mod mcp_servers;
mod models;
mod outcomes;
mod webhook_callbacks;

#[derive(Clone)]
pub struct SqliteDb {
    pub pool: SqlitePool,
}

impl SqliteDb {
    pub async fn connect(path: &str) -> anyhow::Result<Self> {
        // For in-memory databases, use as-is
        if path == ":memory:" {
            let pool = SqlitePool::connect("sqlite::memory:").await?;
            return Ok(Self { pool });
        }
        let expanded = shellexpand::tilde(path).into_owned();
        if let Some(parent) = std::path::Path::new(&expanded).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", expanded))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // Incremental auto-vacuum lets the retention tick return the
            // space deleted rows free to the file system
            // (`MaintenanceRepository::reclaim_space`). The pragma takes
            // effect only on a file created by this open: an existing file
            // keeps its mode until `modelrouter db vacuum` converts it.
            .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
            .foreign_keys(true)
            // SQLite has one writer at a time; a writer that finds the lock held
            // waits up to this long before failing with "database is locked".
            .busy_timeout(std::time::Duration::from_secs(30));
        let pool = SqlitePool::connect_with(opts).await?;
        Ok(Self { pool })
    }
}

/// Rows a retention purge deletes per write transaction. SQLite has a single
/// writer: one DELETE over a large table holds the write lock for its whole
/// run, and every other write (request and cost logging) fails with
/// "database is locked" once it has waited the busy timeout. Short batches,
/// each committed on its own, let those writers in between.
pub(crate) const PURGE_BATCH: usize = 500;

impl SqliteDb {
    /// Deletes the rows of `table` matching `predicate` (a WHERE clause whose
    /// placeholders bind `cutoff`, then `ids`) in batches of `PURGE_BATCH`, each
    /// in its own short transaction, yielding between batches. `detach`, when
    /// given, runs first in each batch with `{ids}` replaced by the batch's row
    /// ids, so rows that reference the deleted ones can drop the reference.
    /// `table`, `predicate` and `detach` are static SQL from this crate; only
    /// the bound values vary. Returns the number of rows deleted.
    pub(crate) async fn delete_in_batches(
        &self,
        table: &str,
        predicate: &str,
        cutoff: &str,
        ids: &[i64],
        detach: Option<&str>,
    ) -> anyhow::Result<u64> {
        let select = format!("SELECT rowid FROM {table} WHERE {predicate} LIMIT {PURGE_BATCH}");
        let mut deleted = 0u64;
        loop {
            let mut tx = self.pool.begin().await?;
            let mut q = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(select.as_str())).bind(cutoff);
            for id in ids {
                q = q.bind(id);
            }
            let batch: Vec<i64> = q.fetch_all(&mut *tx).await?.into_iter().map(|(id,)| id).collect();
            if batch.is_empty() {
                tx.commit().await?;
                return Ok(deleted);
            }
            let placeholders = vec!["?"; batch.len()].join(", ");
            if let Some(detach) = detach {
                let sql = detach.replace("{ids}", &placeholders);
                let mut d = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
                for id in &batch {
                    d = d.bind(id);
                }
                d.execute(&mut *tx).await?;
            }
            let delete = format!("DELETE FROM {table} WHERE rowid IN ({placeholders})");
            let mut d = sqlx::query(sqlx::AssertSqlSafe(delete.as_str()));
            for id in &batch {
                d = d.bind(id);
            }
            deleted += d.execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            if batch.len() < PURGE_BATCH {
                return Ok(deleted);
            }
            tokio::task::yield_now().await;
        }
    }
}

/// Helper: get current UTC timestamp as RFC3339 string
pub(crate) fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}
