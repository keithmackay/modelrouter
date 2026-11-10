//! The schema itself: every backend migrates cleanly from empty, and running
//! the migrations again is a no-op.

use crate::harness::backends;

#[tokio::test]
async fn every_backend_migrates_from_empty() {
    // `backends()` migrates each one; reaching here means they all applied.
    let all = backends().await;
    assert!(!all.is_empty());
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_migrations_are_idempotent() {
    let Ok(url) = std::env::var("MODELROUTER_TEST_POSTGRES_URL") else {
        return;
    };
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;

    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();

    let opts = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new().max_connections(2).connect_with(opts).await.unwrap();
    modelrouter::db::migrations::run_postgres_migrations(&pool).await.unwrap();
    modelrouter::db::migrations::run_postgres_migrations(&pool).await.unwrap();
    pool.close().await;

    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE")).execute(&admin).await.unwrap();
}

/// Every SQLite table and column exists in the Postgres schema too, and vice
/// versa: the repositories on each side are written against the same shape.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_schema_matches_sqlite() {
    use std::collections::BTreeSet;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("MODELROUTER_TEST_POSTGRES_URL") else {
        return;
    };

    let lite = modelrouter::db::sqlite::SqliteDb::connect(":memory:").await.unwrap();
    modelrouter::db::migrations::run_migrations(&lite.pool).await.unwrap();
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
    )
    .fetch_all(&lite.pool)
    .await
    .unwrap();
    let mut sqlite_cols = BTreeSet::new();
    for (table,) in tables {
        let cols: Vec<(String,)> =
            sqlx::query_as(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(&lite.pool)
                .await
                .unwrap();
        for (col,) in cols {
            sqlite_cols.insert(format!("{table}.{col}"));
        }
    }

    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
    let opts = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    modelrouter::db::migrations::run_postgres_migrations(&pool).await.unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name <> '_sqlx_migrations'",
    )
    .bind(&schema)
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE")).execute(&admin).await.unwrap();
    // hook_metrics has no declared key in SQLite (it uses the implicit rowid);
    // Postgres needs an explicit one.
    let pg_cols: BTreeSet<String> = rows
        .into_iter()
        .map(|(t, c)| format!("{t}.{c}"))
        .filter(|c| c != "hook_metrics.id")
        .collect();

    let only_sqlite: Vec<_> = sqlite_cols.difference(&pg_cols).collect();
    let only_pg: Vec<_> = pg_cols.difference(&sqlite_cols).collect();
    assert!(
        only_sqlite.is_empty() && only_pg.is_empty(),
        "schema drift\n  only in sqlite: {only_sqlite:?}\n  only in postgres: {only_pg:?}"
    );
}
