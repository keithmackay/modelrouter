mod common;

use modelrouter::db::models::NewAdminUserFromOidc;
use modelrouter::db::repositories::admin_users::AdminUserRepository;

#[tokio::test]
async fn test_find_by_oidc_subject() {
    let db = common::in_memory_db().await;

    let created = db.create_from_oidc(NewAdminUserFromOidc {
        name: "Alice OIDC".to_string(),
        email: "alice@example.com".to_string(),
        oidc_subject: "google|12345".to_string(),
        role: "admin".to_string(),
    }).await.unwrap();
    assert_eq!(created.password_hash, "");

    let found = db.find_by_oidc_subject("google|12345").await.unwrap().unwrap();
    assert_eq!(found.id, created.id);
    assert_eq!(found.name, "Alice OIDC");
    assert_eq!(found.email.as_deref(), Some("alice@example.com"));
    assert_eq!(found.oidc_subject.as_deref(), Some("google|12345"));
    assert_eq!(found.password_hash, "");
}
