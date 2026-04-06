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

mod oidc_core_tests {
    #[test]
    fn test_oidc_state_store_insert_and_take() {
        use modelrouter::api::admin::oidc::OidcStateStore;
        let store = OidcStateStore::new();
        store.insert("state1".to_string(), "verifier1".to_string());
        let v = store.take("state1");
        assert_eq!(v, Some("verifier1".to_string()));
        // Second take returns None (consumed)
        assert!(store.take("state1").is_none());
    }

    #[test]
    fn test_oidc_pkce_challenge() {
        use modelrouter::api::admin::oidc::{generate_pkce_pair, verify_pkce_challenge};
        let (verifier, challenge) = generate_pkce_pair();
        assert!(verify_pkce_challenge(&verifier, &challenge));
        assert!(!verify_pkce_challenge("wrong", &challenge));
    }

    #[test]
    fn test_oidc_email_allowed() {
        use modelrouter::api::admin::oidc::is_email_allowed;
        let allowed_emails = vec!["alice@example.com".to_string()];
        let allowed_domains = vec!["corp.example.com".to_string()];
        assert!(is_email_allowed("alice@example.com", &allowed_emails, &allowed_domains));
        assert!(is_email_allowed("bob@corp.example.com", &allowed_emails, &allowed_domains));
        assert!(!is_email_allowed("eve@evil.com", &allowed_emails, &allowed_domains));
        // Empty allow-lists = allow all
        assert!(is_email_allowed("anyone@anywhere.com", &[], &[]));
    }
}

mod oidc_integration_tests {
    use modelrouter::db::repositories::admin_users::AdminUserRepository;
    use modelrouter::db::models::{NewAdminUserFromOidc, NewAdminUser};

    #[tokio::test]
    async fn test_create_from_oidc_and_find_by_subject() {
        let db = crate::common::in_memory_db().await;

        let created = db.create_from_oidc(NewAdminUserFromOidc {
            name: "Alice OIDC".to_string(),
            email: "alice@example.com".to_string(),
            oidc_subject: "google|12345".to_string(),
            role: "admin".to_string(),
        }).await.unwrap();

        assert_eq!(created.oidc_subject.as_deref(), Some("google|12345"));
        assert_eq!(created.email.as_deref(), Some("alice@example.com"));
        assert!(created.enabled);
        assert_eq!(created.password_hash, "");

        let found = db.find_by_oidc_subject("google|12345").await.unwrap().unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.name, "Alice OIDC");
        assert_eq!(found.email.as_deref(), Some("alice@example.com"));
        assert_eq!(found.oidc_subject.as_deref(), Some("google|12345"));
    }

    #[tokio::test]
    async fn test_oidc_subject_unique_constraint() {
        let db = crate::common::in_memory_db().await;

        db.create_from_oidc(NewAdminUserFromOidc {
            name: "Alice".to_string(),
            email: "alice@example.com".to_string(),
            oidc_subject: "provider|abc".to_string(),
            role: "admin".to_string(),
        }).await.unwrap();

        // Second insert with same oidc_subject must fail
        let result = db.create_from_oidc(NewAdminUserFromOidc {
            name: "Alice Dup".to_string(),
            email: "alice2@example.com".to_string(),
            oidc_subject: "provider|abc".to_string(),
            role: "admin".to_string(),
        }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_existing_admin_find_returns_oidc_subject_none() {
        let db = crate::common::in_memory_db().await;
        let created = db.create(NewAdminUser {
            name: "bob".to_string(),
            password_hash: "hash".to_string(),
            role: "admin".to_string(),
        }).await.unwrap();

        assert!(created.oidc_subject.is_none());
        assert!(created.email.is_none());
    }
}
