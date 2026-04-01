mod common;

use modelrouter::api::admin::auth::{issue_jwt, verify_jwt, AdminClaims};

#[test]
fn issue_and_verify_jwt_roundtrip() {
    let claims = AdminClaims {
        sub: 1,
        name: "admin".to_string(),
        role: "superadmin".to_string(),
        exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
    };
    let token = issue_jwt(&claims, "test-secret").unwrap();
    let verified = verify_jwt(&token, "test-secret").unwrap();
    assert_eq!(verified.sub, 1);
    assert_eq!(verified.role, "superadmin");
}

#[test]
fn expired_jwt_is_rejected() {
    let claims = AdminClaims {
        sub: 1,
        name: "admin".to_string(),
        role: "superadmin".to_string(),
        exp: 1, // Unix epoch — always expired
    };
    let token = issue_jwt(&claims, "test-secret").unwrap();
    let result = verify_jwt(&token, "test-secret");
    assert!(result.is_err(), "expired token should be rejected");
}

#[test]
fn wrong_secret_is_rejected() {
    let claims = AdminClaims {
        sub: 1,
        name: "admin".to_string(),
        role: "superadmin".to_string(),
        exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
    };
    let token = issue_jwt(&claims, "correct-secret").unwrap();
    let result = verify_jwt(&token, "wrong-secret");
    assert!(result.is_err(), "wrong secret should be rejected");
}
