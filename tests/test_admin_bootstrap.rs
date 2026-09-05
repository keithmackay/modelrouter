use modelrouter::config::schema::{AdminBootstrapConfig, Settings};
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use modelrouter::db::sqlite::SqliteDb;

/// Test: bootstrap creates account when absent
#[tokio::test]
async fn bootstrap_creates_account_when_absent() {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    modelrouter::db::migrations::run_migrations(&db.pool).await.unwrap();

    let bootstrap = AdminBootstrapConfig {
        name: "bootstrapped-admin".to_string(),
        role: "superadmin".to_string(),
        password_hash: bcrypt::hash("test-password", bcrypt::DEFAULT_COST).unwrap(),
    };

    // Validate first
    bootstrap.validate().unwrap();

    // Apply bootstrap (mimics serve startup logic)
    let admin = AdminUserRepository::create(
        &db,
        modelrouter::db::models::NewAdminUser {
            name: bootstrap.name.clone(),
            password_hash: bootstrap.password_hash.clone(),
            role: bootstrap.role.clone(),
        },
    )
    .await
    .unwrap();

    assert_eq!(admin.name, "bootstrapped-admin");
    assert_eq!(admin.role, "superadmin");
    assert!(admin.enabled);

    // Verify password hash stored correctly
    assert!(bcrypt::verify("test-password", &admin.password_hash).unwrap());
}

/// Test: second run is a no-op (password/role unchanged)
#[tokio::test]
async fn bootstrap_second_run_is_noop() {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    modelrouter::db::migrations::run_migrations(&db.pool).await.unwrap();

    let original_hash = bcrypt::hash("original-password", bcrypt::DEFAULT_COST).unwrap();
    let bootstrap = AdminBootstrapConfig {
        name: "admin".to_string(),
        role: "superadmin".to_string(),
        password_hash: original_hash.clone(),
    };

    bootstrap.validate().unwrap();

    // First bootstrap
    AdminUserRepository::create(
        &db,
        modelrouter::db::models::NewAdminUser {
            name: bootstrap.name.clone(),
            password_hash: bootstrap.password_hash.clone(),
            role: bootstrap.role.clone(),
        },
    )
    .await
    .unwrap();

    // Simulate second startup with different password hash
    let new_hash = bcrypt::hash("new-password", bcrypt::DEFAULT_COST).unwrap();
    let second_bootstrap = AdminBootstrapConfig {
        name: "admin".to_string(),
        role: "viewer".to_string(),
        password_hash: new_hash,
    };

    // Check if account exists
    let existing = AdminUserRepository::find_by_name(&db, &second_bootstrap.name)
        .await
        .unwrap();
    assert!(existing.is_some());

    let existing = existing.unwrap();
    // Password and role should NOT have changed
    assert_eq!(existing.password_hash, original_hash);
    assert_eq!(existing.role, "superadmin");
    assert!(bcrypt::verify("original-password", &existing.password_hash).unwrap());
    assert!(!bcrypt::verify("new-password", &existing.password_hash).unwrap());
}

/// Test: invalid role fails validation
#[test]
fn bootstrap_invalid_role_fails() {
    let bootstrap = AdminBootstrapConfig {
        name: "admin".to_string(),
        role: "god-mode".to_string(),
        password_hash: bcrypt::hash("password", bcrypt::DEFAULT_COST).unwrap(),
    };

    let result = bootstrap.validate();
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("god-mode"));
    assert!(err.contains("superadmin"));
    assert!(err.contains("viewer"));
}

/// Test: malformed bcrypt hash fails validation
#[test]
fn bootstrap_malformed_hash_fails() {
    let bootstrap = AdminBootstrapConfig {
        name: "admin".to_string(),
        role: "superadmin".to_string(),
        password_hash: "not-a-bcrypt-hash".to_string(),
    };

    let result = bootstrap.validate();
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("bcrypt"));
}

/// Test: viewer role is accepted
#[test]
fn bootstrap_viewer_role_valid() {
    let bootstrap = AdminBootstrapConfig {
        name: "view-only-admin".to_string(),
        role: "viewer".to_string(),
        password_hash: bcrypt::hash("password", bcrypt::DEFAULT_COST).unwrap(),
    };

    assert!(bootstrap.validate().is_ok());
}

/// Test: config section absent = no-op (Settings default)
#[test]
fn bootstrap_section_absent_is_noop() {
    let toml = r#"
        [server]
        host = "127.0.0.1"
    "#;

    let settings: Settings = toml::from_str(toml).unwrap();
    assert!(settings.admin.bootstrap.is_none());
}

/// Test: config section parses correctly
#[test]
fn bootstrap_section_parses() {
    let hash = bcrypt::hash("test", bcrypt::DEFAULT_COST).unwrap();
    let toml = format!(
        r#"
        [admin.bootstrap]
        name = "root"
        role = "superadmin"
        password_hash = "{}"
        "#,
        hash
    );

    let settings: Settings = toml::from_str(&toml).unwrap();
    assert!(settings.admin.bootstrap.is_some());

    let bootstrap = settings.admin.bootstrap.unwrap();
    assert_eq!(bootstrap.name, "root");
    assert_eq!(bootstrap.role, "superadmin");
    assert_eq!(bootstrap.password_hash, hash);
    assert!(bootstrap.validate().is_ok());
}
