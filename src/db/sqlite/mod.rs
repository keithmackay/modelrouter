use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::str::FromStr;

mod groups;
mod users;
mod admin_users;
mod api_keys;
mod sessions;
mod prompts;
mod costs;
mod budgets;
mod audit;
mod hooks;
mod rate_limits;
mod mcp_servers;

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
            .foreign_keys(true);
        let pool = SqlitePool::connect_with(opts).await?;
        Ok(Self { pool })
    }
}

/// Helper: get current UTC timestamp as RFC3339 string
pub(crate) fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}
