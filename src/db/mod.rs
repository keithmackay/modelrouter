pub mod migrations;
pub mod models;
pub mod prompt_store;
pub mod repositories;
pub mod retention;
pub mod sqlite;
#[cfg(feature = "postgres")]
pub mod postgres;

use std::sync::Arc;

use crate::api::app::DatabaseProvider;
use crate::config::schema::DatabaseConfig;

/// The main database a command runs against: connected and migrated.
pub struct MainDb {
    pub db: Arc<dyn DatabaseProvider>,
    /// The SQLite database behind `db`, for the CLI reports and dashboard
    /// panels that still query SQLite directly. `None` on Postgres.
    pub sqlite: Option<sqlite::SqliteDb>,
}

impl MainDb {
    /// The SQLite pool for a feature that has no Postgres implementation yet;
    /// on Postgres, an error naming that feature.
    pub fn sqlite_pool(&self, feature: &str) -> anyhow::Result<&sqlx::SqlitePool> {
        self.sqlite.as_ref().map(|db| &db.pool).ok_or_else(|| {
            anyhow::anyhow!("{feature} is not available on the Postgres backend yet")
        })
    }
}

/// Open the configured main database and bring its schema up to date:
/// Postgres when `database.postgres_url` is set, else the SQLite file at
/// `database.path`.
pub async fn open(config: &DatabaseConfig) -> anyhow::Result<MainDb> {
    match postgres_url(config) {
        Some(url) => open_postgres(url).await,
        None => {
            let db = sqlite::SqliteDb::connect(&config.path).await?;
            migrations::run_migrations(&db.pool).await?;
            Ok(MainDb { db: Arc::new(db.clone()), sqlite: Some(db) })
        }
    }
}

/// The Postgres URL, when one is configured. An empty string counts as unset,
/// so `MODELROUTER_DATABASE__POSTGRES_URL=` can switch an override off.
fn postgres_url(config: &DatabaseConfig) -> Option<&str> {
    config.postgres_url.as_deref().map(str::trim).filter(|u| !u.is_empty())
}

#[cfg(feature = "postgres")]
async fn open_postgres(url: &str) -> anyhow::Result<MainDb> {
    let db = postgres::PostgresDb::connect(url).await?;
    migrations::run_postgres_migrations(&db.pool).await?;
    Ok(MainDb { db: Arc::new(db), sqlite: None })
}

/// A configured Postgres URL in a build without Postgres support is a hard
/// error: silently falling back to SQLite would split the data across two
/// databases.
#[cfg(not(feature = "postgres"))]
async fn open_postgres(_url: &str) -> anyhow::Result<MainDb> {
    anyhow::bail!(
        "database.postgres_url is set, but this binary was built without the \
         `postgres` feature; rebuild with `--features postgres` or unset it"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::users::UserRepository;

    fn config(postgres_url: Option<&str>) -> DatabaseConfig {
        DatabaseConfig { path: ":memory:".to_string(), postgres_url: postgres_url.map(String::from) }
    }

    #[tokio::test]
    async fn opens_migrated_sqlite_without_a_postgres_url() {
        let main = open(&config(None)).await.unwrap();
        assert!(main.sqlite_pool("report").is_ok());
        // Migrated: a repository query runs.
        assert!(UserRepository::list(&*main.db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn blank_postgres_url_means_sqlite() {
        let main = open(&config(Some("  "))).await.unwrap();
        assert!(main.sqlite.is_some());
    }

    #[cfg(not(feature = "postgres"))]
    #[tokio::test]
    async fn postgres_url_without_the_feature_is_an_error() {
        let err = open(&config(Some("postgres://x@localhost/db"))).await.err().unwrap();
        assert!(err.to_string().contains("--features postgres"), "{err}");
    }

    #[test]
    fn sqlite_pool_names_the_feature_on_postgres() {
        // A MainDb without SQLite stands in for a Postgres one.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let lite = rt.block_on(sqlite::SqliteDb::connect(":memory:")).unwrap();
        let main = MainDb { db: Arc::new(lite), sqlite: None };
        let err = main.sqlite_pool("`report usage`").unwrap_err();
        assert_eq!(err.to_string(), "`report usage` is not available on the Postgres backend yet");
    }
}
