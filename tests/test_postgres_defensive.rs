//! Postgres-specific defensive query tests (issue #50).
//!
//! These tests verify that malformed attribution_tags rows (e.g., JSON arrays
//! instead of objects) don't crash queries that expect objects. They require a
//! live Postgres instance and may be `#[ignore]`d in CI if no DB is available.

#![cfg(feature = "postgres")]

use modelrouter::db::repositories::costs::CostRepository;

/// Helper to insert a cost row with a given attribution_tags value directly.
async fn insert_row_with_tags(pool: &sqlx::PgPool, tags: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    // Insert a minimal user if not exists
    sqlx::query(
        "INSERT INTO users (name, enabled, created_at, metadata) \
         VALUES ('test-user', true, $1, '{}') ON CONFLICT (name) DO NOTHING",
    )
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();

    let user_id: (i64,) = sqlx::query_as("SELECT id FROM users WHERE name = 'test-user'")
        .fetch_one(pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO cost_ledger (user_id, model, provider, tokens_in, tokens_out, cost_usd, created_at, attribution_tags) \
         VALUES ($1, 'test-model', 'test-provider', 100, 50, 0.01, $2, $3)",
    )
    .bind(user_id.0)
    .bind(&now)
    .bind(tags)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore] // Requires live Postgres; run with `cargo test --features postgres -- --ignored`
async fn distinct_attribution_tag_keys_tolerates_array_rows() {
    use modelrouter::db::postgres::PostgresDb;

    // Connect to a test database. Adjust this URL for your CI/local setup.
    let database_url =
        std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/modelrouter_test".to_string());
    let db = PostgresDb::connect(&database_url).await.unwrap();

    // Clean up any existing test data
    sqlx::query("DELETE FROM cost_ledger WHERE model = 'test-model'")
        .execute(&db.pool)
        .await
        .unwrap();

    // Insert one well-formed object row and one malformed array row (issue #50).
    insert_row_with_tags(&db.pool, r#"{"engagement":"A"}"#).await;
    insert_row_with_tags(&db.pool, "[]").await;

    // Before the fix, this query would raise "cannot call jsonb_object_keys on
    // an array" and surface as a 500. After the fix, the array row is skipped.
    let keys = CostRepository::distinct_attribution_tag_keys(&db)
        .await
        .unwrap();

    assert_eq!(keys, vec!["engagement".to_string()]);

    // Clean up
    sqlx::query("DELETE FROM cost_ledger WHERE model = 'test-model'")
        .execute(&db.pool)
        .await
        .unwrap();
}
