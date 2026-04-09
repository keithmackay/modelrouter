// tests/test_key_expiry.rs
use modelrouter::db::models::ApiKey;

fn make_key(expires_at: Option<String>) -> ApiKey {
    ApiKey {
        id: 1,
        user_id: 1,
        key_hash: "abc".to_string(),
        label: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        expires_at,
        project: None,
        disabled_at: None,
    }
}

#[test]
fn key_without_expiry_is_valid() {
    assert!(make_key(None).is_valid());
}

#[test]
fn key_with_future_expiry_is_valid() {
    let future = "2099-12-31T23:59:59+00:00".to_string();
    assert!(make_key(Some(future)).is_valid());
}

#[test]
fn key_with_past_expiry_is_expired() {
    let past = "2020-01-01T00:00:00+00:00".to_string();
    assert!(!make_key(Some(past)).is_valid());
}

#[test]
fn disabled_key_is_invalid_regardless_of_expiry() {
    let mut key = make_key(None);
    key.enabled = false;
    assert!(!key.is_valid());
}
