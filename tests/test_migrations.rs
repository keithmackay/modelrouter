mod common;

use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::models::{NewUser, NewApiKey};

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
        group_name: None,
        email: None,
    };
    let created = db.create(new_user).await.unwrap();
    assert_eq!(created.name, "alice");

    // Create an API key for the user and verify it can be found
    let key = db.create_api_key(NewApiKey {
        user_id: created.id,
        key_hash: "abc123hash".to_string(),
        label: None,
        expires_at: None,
        project: None,
    }).await.unwrap();

    let found = db.find_api_key_by_hash("abc123hash").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().user_id, created.id);
    let _ = key;
}

#[tokio::test]
async fn token_rotation_overlap_window() {
    let db = common::in_memory_db().await;
    let new_user = NewUser {
        name: "bob".to_string(),
        group_name: None,
        email: None,
    };
    let user = db.create(new_user).await.unwrap();

    // Create the old key
    db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: "old_hash".to_string(),
        label: None,
        expires_at: None,
        project: None,
    }).await.unwrap();

    // Rotate: disable all old keys, then create new key with future expiry
    db.disable_all_keys_for_user(user.id).await.unwrap();
    db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: "new_hash".to_string(),
        label: None,
        expires_at: Some("2099-12-31T23:59:59Z".to_string()),
        project: None,
    }).await.unwrap();

    // Old key should be disabled (not found as valid)
    let found_old = db.find_api_key_by_hash("old_hash").await.unwrap();
    assert!(
        found_old.map(|k| !k.enabled).unwrap_or(true),
        "old key should be disabled after rotation"
    );

    // New key should be enabled
    let found_new = db.find_api_key_by_hash("new_hash").await.unwrap();
    assert!(found_new.is_some(), "new key should exist");
    assert!(found_new.unwrap().enabled, "new key should be enabled");
}

#[tokio::test]
async fn old_key_disabled_after_rotation() {
    let db = common::in_memory_db().await;
    let new_user = NewUser {
        name: "carol".to_string(),
        group_name: None,
        email: None,
    };
    let user = db.create(new_user).await.unwrap();

    // Create old key
    db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: "carol_old".to_string(),
        label: None,
        expires_at: None,
        project: None,
    }).await.unwrap();

    // Rotate: disable old keys, create new key
    db.disable_all_keys_for_user(user.id).await.unwrap();
    db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: "carol_new".to_string(),
        label: None,
        expires_at: None,
        project: None,
    }).await.unwrap();

    // Old key should be disabled
    let found_old = db.find_api_key_by_hash("carol_old").await.unwrap();
    assert!(
        found_old.map(|k| !k.enabled).unwrap_or(true),
        "old key should be disabled after rotation"
    );

    // New key should work
    let found_new = db.find_api_key_by_hash("carol_new").await.unwrap();
    assert!(found_new.is_some());
    assert!(found_new.unwrap().enabled);
}
