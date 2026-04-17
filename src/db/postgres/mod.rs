#![cfg(feature = "postgres")]

mod groups;
mod users;
mod admin_users;
mod sessions;
mod prompts;
mod costs;
mod budgets;
mod audit;
mod hooks;
mod rate_limits;
mod api_keys;
mod mcp_servers;
mod models;

use sqlx::PgPool;

#[derive(Clone)]
pub struct PostgresDb {
    pub pool: PgPool,
}

impl PostgresDb {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPool::connect(url).await?;
        Ok(Self { pool })
    }
}

/// Helper: get current UTC timestamp as RFC3339 string
pub(crate) fn now_utc() -> String {
    chrono::Utc::now().to_rfc3339()
}
