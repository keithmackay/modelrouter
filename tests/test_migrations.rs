mod common;

use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::models::NewUser;

#[tokio::test]
async fn migrations_create_all_tables() {
    let db = common::in_memory_db().await;
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let names: Vec<&str> = tables.iter().map(|(s,)| s.as_str()).collect();
    let expected = ["admin_users", "audit_log", "budget_rules", "cost_ledger",
                    "hook_metrics", "hook_permissions", "prompts", "rate_limit_state",
                    "sessions", "users"];
    for table in &expected {
        assert!(names.contains(table), "missing table: {}", table);
    }
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = common::in_memory_db().await;
    // Running migrations again should not fail
    modelrouter::db::migrations::run_migrations(&db.pool).await.unwrap();
}

#[tokio::test]
async fn create_and_find_user() {
    let db = common::in_memory_db().await;
    let new_user = NewUser {
        name: "alice".to_string(),
        api_key_hash: "abc123hash".to_string(),
        group_name: None,
    };
    let created = db.create(new_user).await.unwrap();
    assert_eq!(created.name, "alice");

    let found = db.find_by_api_key("abc123hash").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "alice");
}

#[tokio::test]
async fn token_rotation_overlap_window() {
    let db = common::in_memory_db().await;
    let new_user = NewUser {
        name: "bob".to_string(),
        api_key_hash: "old_hash".to_string(),
        group_name: None,
    };
    let user = db.create(new_user).await.unwrap();

    // Rotate key with future expiry
    let future_expiry = "2099-12-31T23:59:59Z";
    db.rotate_key(user.id, "new_hash", future_expiry).await.unwrap();

    // Old key should still work within overlap window
    let found = db.find_by_api_key("old_hash").await.unwrap();
    assert!(found.is_some(), "old key should work in overlap window");

    // New key should also work
    let found_new = db.find_by_api_key("new_hash").await.unwrap();
    assert!(found_new.is_some(), "new key should work");
}

#[tokio::test]
async fn old_key_rejected_after_expiry() {
    let db = common::in_memory_db().await;
    let new_user = NewUser {
        name: "carol".to_string(),
        api_key_hash: "carol_old".to_string(),
        group_name: None,
    };
    let user = db.create(new_user).await.unwrap();

    // Rotate with PAST expiry
    let past_expiry = "2000-01-01T00:00:00Z";
    db.rotate_key(user.id, "carol_new", past_expiry).await.unwrap();

    // Old key should be rejected
    let found = db.find_by_api_key("carol_old").await.unwrap();
    assert!(found.is_none(), "expired old key should be rejected");

    // New key should work
    let found_new = db.find_by_api_key("carol_new").await.unwrap();
    assert!(found_new.is_some());
}
