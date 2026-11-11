//! Backend fixtures for the conformance tests.

use std::sync::Arc;

use modelrouter::api::app::DatabaseProvider;

/// One backend under test: a fresh, migrated database.
pub struct Backend {
    pub name: &'static str,
    pub db: Arc<dyn DatabaseProvider>,
    /// The same database as a raw pool, for `modelrouter::report`.
    pub reports: modelrouter::report::ReportPool,
    #[cfg(feature = "postgres")]
    _schema: Option<PgSchema>,
}

/// Every backend this run covers, each freshly migrated and empty.
pub async fn backends() -> Vec<Backend> {
    let mut out = vec![sqlite().await];
    #[cfg(feature = "postgres")]
    if let Some(pg) = postgres().await {
        out.push(pg);
    }
    out
}

/// Run `scenario` against every backend. A failure names the backend, so a
/// Postgres-only divergence reads as one.
pub async fn each_backend<F, Fut>(scenario: F)
where
    F: Fn(Arc<dyn DatabaseProvider>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for backend in backends().await {
        eprintln!("-- backend: {}", backend.name);
        scenario(backend.db.clone()).await;
    }
}

/// [`each_backend`] for scenarios that also read through the SQL reports.
pub async fn each_backend_with_reports<F, Fut>(scenario: F)
where
    F: Fn(Arc<dyn DatabaseProvider>, modelrouter::report::ReportPool) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for backend in backends().await {
        eprintln!("-- backend: {}", backend.name);
        scenario(backend.db.clone(), backend.reports.clone()).await;
    }
}

async fn sqlite() -> Backend {
    let db = modelrouter::db::sqlite::SqliteDb::connect(":memory:")
        .await
        .expect("open in-memory sqlite");
    modelrouter::db::migrations::run_migrations(&db.pool)
        .await
        .expect("migrate sqlite");
    Backend {
        name: "sqlite",
        reports: modelrouter::report::ReportPool::from(&db.pool),
        db: Arc::new(db),
        #[cfg(feature = "postgres")]
        _schema: None,
    }
}

/// A per-test Postgres schema, dropped on a background thread when the test's
/// `Backend` goes out of scope.
#[cfg(feature = "postgres")]
struct PgSchema {
    url: String,
    name: String,
}

#[cfg(feature = "postgres")]
impl Drop for PgSchema {
    fn drop(&mut self) {
        let url = self.url.clone();
        let name = self.name.clone();
        // Drop runs inside the test's runtime; a fresh thread with its own
        // runtime can block on the cleanup without nesting runtimes.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Ok(pool) = sqlx::PgPool::connect(&url).await {
                    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {name} CASCADE"))
                        .execute(&pool)
                        .await;
                }
            });
        })
        .join();
    }
}

#[cfg(feature = "postgres")]
async fn postgres() -> Option<Backend> {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;

    let url = std::env::var("MODELROUTER_TEST_POSTGRES_URL").ok()?;
    let name = format!("t_{}", uuid::Uuid::new_v4().simple());

    let admin = sqlx::PgPool::connect(&url).await.expect("connect to test postgres");
    sqlx::query(&format!("CREATE SCHEMA {name}"))
        .execute(&admin)
        .await
        .expect("create test schema");
    admin.close().await;

    let opts = PgConnectOptions::from_str(&url)
        .expect("parse MODELROUTER_TEST_POSTGRES_URL")
        .options([("search_path", name.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .expect("connect to test schema");
    modelrouter::db::migrations::run_postgres_migrations(&pool)
        .await
        .expect("migrate postgres");

    Some(Backend {
        name: "postgres",
        reports: modelrouter::report::ReportPool::from(pool.clone()),
        db: Arc::new(modelrouter::db::postgres::PostgresDb { pool }),
        _schema: Some(PgSchema { url, name }),
    })
}
