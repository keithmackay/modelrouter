//! Postgres coverage for the comparison-arm queries.
//!
//! These tests need a live database and are `#[ignore]`d by default:
//!
//! ```text
//! MODELROUTER_TEST_POSTGRES_URL=postgres://user:pass@localhost/modelrouter_test \
//!     cargo test --features postgres --test test_compare_postgres -- --ignored
//! ```
//!
//! Each test tags its rows with a unique token (model/provider/correlation id)
//! so runs against a shared database neither collide nor need a clean slate,
//! and deletes those rows on the way out.
#![cfg(feature = "postgres")]

use modelrouter::db::postgres::PostgresDb;
use modelrouter::db::repositories::costs::{ArmFilter, AttributionFilter, CostRepository};

const W_START: &str = "2026-03-01T00:00:00Z";
const W_END: &str = "2026-04-01T00:00:00Z";

async fn connect() -> PostgresDb {
    let url = std::env::var("MODELROUTER_TEST_POSTGRES_URL")
        .expect("MODELROUTER_TEST_POSTGRES_URL must point at a scratch Postgres database");
    let db = PostgresDb::connect(&url).await.expect("connect to postgres");
    sqlx::migrate!("./migrations/postgres")
        .run(&db.pool)
        .await
        .expect("apply postgres migrations");
    db
}

/// Unique-per-run token so parallel or repeated runs never see each other's rows.
fn token(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{}-{}", prefix, std::process::id(), nanos)
}

async fn create_user(db: &PostgresDb, name: &str) -> i64 {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO users (name, enabled, created_at, metadata) \
         VALUES ($1, TRUE, '2026-01-01T00:00:00Z', '{}') RETURNING id",
    )
    .bind(name)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    id
}

#[allow(clippy::too_many_arguments)]
async fn insert_ledger(
    db: &PostgresDb,
    user_id: i64,
    model: &str,
    provider: &str,
    run: Option<&str>,
    tags: &str,
    cache_hit: bool,
    cost_usd: f64,
    created_at: &str,
) {
    sqlx::query(
        "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, \
         tokens_in, tokens_out, cost_usd, saved_usd, cache_hit, \
         attribution_correlation_id, attribution_tags, created_at) \
         VALUES ($1, NULL, $2, $3, NULL, 10, 5, $4, 0.0, $5, $6, $7, $8)",
    )
    .bind(user_id)
    .bind(model)
    .bind(provider)
    .bind(cost_usd)
    .bind(cache_hit)
    .bind(run)
    .bind(tags)
    .bind(created_at)
    .execute(&db.pool)
    .await
    .unwrap();
}

async fn cleanup_user(db: &PostgresDb, user_id: i64) {
    sqlx::query("DELETE FROM cost_ledger WHERE user_id = $1")
        .bind(user_id)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn arm_totals_by_model_and_provider() {
    let db = connect().await;
    let user = create_user(&db, &token("u")).await;
    let x = token("model-x");
    let y = token("model-y");
    let p1 = token("prov-1");
    let p2 = token("prov-2");

    insert_ledger(&db, user, &x, &p1, None, "{}", false, 1.0, "2026-03-02T00:00:00Z").await;
    insert_ledger(&db, user, &x, &p1, None, "{}", true, 0.0, "2026-03-03T00:00:00Z").await;
    insert_ledger(&db, user, &x, &p2, None, "{}", false, 2.0, "2026-03-04T00:00:00Z").await;
    insert_ledger(&db, user, &y, &p1, None, "{}", false, 4.0, "2026-03-05T00:00:00Z").await;
    insert_ledger(&db, user, &x, &p1, None, "{}", false, 8.0, "2026-02-28T23:59:59Z").await; // outside

    let tx = db.arm_totals(&ArmFilter::Model(x.clone()), W_START, W_END).await.unwrap();
    assert_eq!(tx.requests, 3);
    assert_eq!(tx.cost_usd, 3.0);
    assert_eq!(tx.cache_hits, 1);

    let tp1 = db.arm_totals(&ArmFilter::Provider(p1.clone()), W_START, W_END).await.unwrap();
    assert_eq!(tp1.requests, 3);
    assert_eq!(tp1.cost_usd, 5.0);

    let days = db.arm_by_day(&ArmFilter::Model(x.clone()), W_START, W_END).await.unwrap();
    assert_eq!(days.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(), vec!["2026-03-02", "2026-03-03", "2026-03-04"]);

    let by_model = db.arm_by_model(&ArmFilter::Provider(p1.clone()), W_START, W_END).await.unwrap();
    assert_eq!(by_model[0].key, y);
    assert_eq!(by_model[1].key, x);

    cleanup_user(&db, user).await;
}

#[tokio::test]
#[ignore]
async fn arm_totals_by_tag_uses_bound_json_path() {
    let db = connect().await;
    let user = create_user(&db, &token("u")).await;
    let model = token("model");
    let arm_value = token("arm-a");
    let other_value = token("arm-b");

    let tag = |v: &str| format!(r#"{{"arm":"{}"}}"#, v);
    insert_ledger(&db, user, &model, "p", None, &tag(&arm_value), false, 1.0, "2026-03-02T00:00:00Z").await;
    insert_ledger(&db, user, &model, "p", None, &tag(&arm_value), false, 2.0, "2026-03-03T00:00:00Z").await;
    insert_ledger(&db, user, &model, "p", None, &tag(&other_value), false, 4.0, "2026-03-04T00:00:00Z").await;
    insert_ledger(&db, user, &model, "p", None, "{}", false, 8.0, "2026-03-05T00:00:00Z").await;

    let filter = ArmFilter::Attribution(AttributionFilter::Tag {
        key: "arm".to_string(),
        value: arm_value.clone(),
    });
    let t = db.arm_totals(&filter, W_START, W_END).await.unwrap();
    assert_eq!(t.requests, 2);
    assert_eq!(t.cost_usd, 3.0);

    // A key containing characters that would matter if interpolated into a
    // JSON path still round-trips because the key is bound, not inlined.
    let odd_key = "a.b:c-d";
    insert_ledger(
        &db,
        user,
        &model,
        "p",
        None,
        &format!(r#"{{"{}":"{}"}}"#, odd_key, arm_value),
        false,
        16.0,
        "2026-03-06T00:00:00Z",
    )
    .await;
    let odd = ArmFilter::Attribution(AttributionFilter::Tag {
        key: odd_key.to_string(),
        value: arm_value.clone(),
    });
    let t = db.arm_totals(&odd, W_START, W_END).await.unwrap();
    assert_eq!(t.requests, 1);
    assert_eq!(t.cost_usd, 16.0);

    cleanup_user(&db, user).await;
}

#[tokio::test]
#[ignore]
async fn recent_correlation_ids_and_providers_pickers() {
    let db = connect().await;
    let user = create_user(&db, &token("u")).await;
    let model = token("model");
    let provider = token("prov");
    let old = token("run-old");
    let new = token("run-new");

    insert_ledger(&db, user, &model, &provider, Some(&old), "{}", false, 1.0, "2026-03-02T00:00:00Z").await;
    insert_ledger(&db, user, &model, &provider, Some(&new), "{}", false, 1.0, "2026-03-03T00:00:00Z").await;
    insert_ledger(&db, user, &model, &provider, Some(&old), "{}", false, 1.0, "2026-03-04T00:00:00Z").await;
    insert_ledger(&db, user, &model, &provider, None, "{}", false, 1.0, "2099-01-01T00:00:00Z").await;

    // Other tests in this binary may be inserting concurrently, so check the
    // relative order of our two ids rather than the exact head of the list.
    let ids = db.distinct_recent_correlation_ids(10_000).await.unwrap();
    let ours: Vec<&String> = ids.iter().filter(|id| **id == old || **id == new).collect();
    // `old` has the newest row (03-04) of the two, so it comes first.
    assert_eq!(ours, vec![&old, &new]);

    let providers = db.distinct_providers_in_ledger().await.unwrap();
    assert!(providers.contains(&provider));
    assert!(providers.windows(2).all(|w| w[0] <= w[1]), "providers must be sorted");

    cleanup_user(&db, user).await;
}
