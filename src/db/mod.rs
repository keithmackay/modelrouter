pub mod migrations;
pub mod models;
pub mod prompt_store;
pub mod repositories;
pub mod retention;
pub mod sqlite;
pub mod transfer;

use std::sync::Arc;

use crate::api::app::DatabaseProvider;
use crate::config::schema::DatabaseConfig;

/// The configured store, opened and migrated. One of these backs `serve` and
/// every CLI command, so the backend is chosen in exactly one place.
pub struct Store {
    pub db: Arc<dyn DatabaseProvider>,
    /// The raw pool, for the schema-driven paths (export, import) and the
    /// few reports that still query SQLite directly.
    pub pool: AnyPool,
    pub backend: &'static str,
}

pub enum AnyPool {
    Sqlite(sqlx::SqlitePool),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
}

impl Store {
    pub fn sqlite_pool_opt(&self) -> Option<&sqlx::SqlitePool> {
        match &self.pool {
            AnyPool::Sqlite(pool) => Some(pool),
            #[cfg(feature = "postgres")]
            AnyPool::Postgres(_) => None,
        }
    }

    /// The SQLite pool, or a clear error naming the feature that still
    /// needs it when the store is PostgreSQL.
    pub fn sqlite_pool(&self, needed_for: &str) -> anyhow::Result<&sqlx::SqlitePool> {
        self.sqlite_pool_opt().ok_or_else(|| {
            anyhow::anyhow!(
                "{needed_for} is not available on the {} backend yet",
                self.backend
            )
        })
    }
}

/// Open the store `[database]` names and bring its schema to head.
/// `postgres_url` selects PostgreSQL when set; a binary built without the
/// `postgres` feature refuses such a config instead of silently opening
/// SQLite next to it.
pub async fn open(cfg: &DatabaseConfig) -> anyhow::Result<Store> {
    match cfg
        .postgres_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(url) => open_postgres(url).await,
        None => {
            let db = sqlite::SqliteDb::connect(&cfg.path).await?;
            migrations::run_migrations(&db.pool).await?;
            Ok(Store {
                pool: AnyPool::Sqlite(db.pool.clone()),
                db: Arc::new(db),
                backend: "sqlite",
            })
        }
    }
}

#[cfg(feature = "postgres")]
async fn open_postgres(url: &str) -> anyhow::Result<Store> {
    let db = postgres::PostgresDb::connect(url).await?;
    migrations::run_postgres_migrations(&db.pool).await?;
    Ok(Store {
        pool: AnyPool::Postgres(db.pool.clone()),
        db: Arc::new(db),
        backend: "postgres",
    })
}

#[cfg(not(feature = "postgres"))]
async fn open_postgres(_url: &str) -> anyhow::Result<Store> {
    anyhow::bail!(
        "`database.postgres_url` is set, but this binary was built without the `postgres` \
         cargo feature; rebuild with `cargo build --release --features postgres` or unset it"
    )
}
#[cfg(feature = "postgres")]
pub mod postgres;
