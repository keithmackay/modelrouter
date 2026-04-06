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

#[cfg(test)]
mod oidc_config_tests {
    use modelrouter::config::schema::Settings;

    #[test]
    fn test_oidc_config_defaults() {
        let settings: Settings = toml::from_str("").unwrap();
        assert!(!settings.oidc.enabled);
        assert_eq!(settings.oidc.auto_provision_role, "admin");
        assert!(settings.oidc.allowed_emails.is_empty());
        assert!(settings.oidc.allowed_domains.is_empty());
    }

    #[test]
    fn test_oidc_config_full_parse() {
        let toml_str = r#"
[oidc]
enabled = true
issuer_url = "https://accounts.google.com"
client_id = "my-client-id"
client_secret = "my-secret"
redirect_uri = "http://localhost:8080/admin/auth/oidc/callback"
allowed_emails = ["alice@example.com"]
allowed_domains = ["example.com"]
auto_provision_role = "superadmin"
"#;
        let settings: Settings = toml::from_str(toml_str).unwrap();
        assert!(settings.oidc.enabled);
        assert_eq!(settings.oidc.issuer_url, "https://accounts.google.com");
        assert_eq!(settings.oidc.client_id, "my-client-id");
        assert_eq!(settings.oidc.allowed_domains, vec!["example.com"]);
        assert_eq!(settings.oidc.auto_provision_role, "superadmin");
    }
}
