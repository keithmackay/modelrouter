use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::GatewayBootstrapConfig;
use modelrouter::db::models::NewApiKey;
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::sqlite::SqliteDb;

async fn fresh_db() -> SqliteDb {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    modelrouter::db::migrations::run_migrations(&db.pool).await.unwrap();
    db
}

fn with_key(user: &str, key: &str) -> GatewayBootstrapConfig {
    GatewayBootstrapConfig {
        user: user.to_string(),
        key: Some(key.to_string()),
        key_hash: None,
        label: "bootstrap".to_string(),
    }
}

#[tokio::test]
async fn creates_user_and_key_on_a_fresh_database() {
    let db = fresh_db().await;
    with_key("svc", "mr-seeded-key").apply(&db).await.unwrap();

    let key = db.find_api_key_by_hash(&hash_token("mr-seeded-key")).await.unwrap()
        .expect("seeded key is usable");
    let user = UserRepository::find_by_name(&db, "svc").await.unwrap().unwrap();
    assert_eq!(key.user_id, user.id);
    assert!(key.enabled);
    assert_eq!(key.label.as_deref(), Some("bootstrap"));
}

#[tokio::test]
async fn key_hash_seeds_the_same_lookup_as_the_raw_key() {
    let db = fresh_db().await;
    let cfg = GatewayBootstrapConfig {
        user: "svc".to_string(),
        key: None,
        key_hash: Some(hash_token("mr-only-the-hash-is-configured")),
        label: "bootstrap".to_string(),
    };
    cfg.apply(&db).await.unwrap();
    assert!(db.find_api_key_by_hash(&hash_token("mr-only-the-hash-is-configured")).await.unwrap().is_some());
}

#[tokio::test]
async fn second_start_is_a_noop() {
    let db = fresh_db().await;
    let cfg = with_key("svc", "mr-seeded-key");
    cfg.apply(&db).await.unwrap();
    cfg.apply(&db).await.unwrap();

    assert_eq!(UserRepository::list(&db).await.unwrap().len(), 1);
    assert_eq!(db.list_all_api_keys().await.unwrap().len(), 1);
}

#[tokio::test]
async fn existing_user_gets_the_key_added() {
    let db = fresh_db().await;
    let user = UserRepository::create(&db, modelrouter::db::models::NewUser { name: "svc".into(), email: None })
        .await.unwrap();
    with_key("svc", "mr-seeded-key").apply(&db).await.unwrap();

    let key = db.find_api_key_by_hash(&hash_token("mr-seeded-key")).await.unwrap().unwrap();
    assert_eq!(key.user_id, user.id);
    assert_eq!(UserRepository::list(&db).await.unwrap().len(), 1);
}

#[tokio::test]
async fn rotation_adds_the_new_key_and_leaves_the_old_one() {
    let db = fresh_db().await;
    with_key("svc", "mr-old").apply(&db).await.unwrap();
    with_key("svc", "mr-new").apply(&db).await.unwrap();

    assert!(db.find_api_key_by_hash(&hash_token("mr-old")).await.unwrap().is_some());
    assert!(db.find_api_key_by_hash(&hash_token("mr-new")).await.unwrap().is_some());
}

#[tokio::test]
async fn a_disabled_key_stays_disabled() {
    let db = fresh_db().await;
    let cfg = with_key("svc", "mr-seeded-key");
    cfg.apply(&db).await.unwrap();
    let key = db.find_api_key_by_hash(&hash_token("mr-seeded-key")).await.unwrap().unwrap();
    db.disable_key(key.id).await.unwrap();

    cfg.apply(&db).await.unwrap();

    assert!(db.find_api_key_by_hash(&hash_token("mr-seeded-key")).await.unwrap().is_none());
    assert_eq!(db.list_all_api_keys().await.unwrap().len(), 1, "no second row for the same hash");
}

#[tokio::test]
async fn a_key_held_by_another_user_refuses_to_start() {
    let db = fresh_db().await;
    let other = UserRepository::create(&db, modelrouter::db::models::NewUser { name: "other".into(), email: None })
        .await.unwrap();
    db.create_api_key(NewApiKey {
        user_id: other.id,
        key_hash: hash_token("mr-shared"),
        label: None,
        expires_at: None,
        project: None,
        session_window_secs: None,
    }).await.unwrap();

    let err = with_key("svc", "mr-shared").apply(&db).await.unwrap_err();
    assert!(err.to_string().contains("another user"), "{err}");
}

#[test]
fn malformed_configs_refuse_to_start() {
    let base = with_key("svc", "k");
    let cases = [
        GatewayBootstrapConfig { key: None, ..base.clone() },
        GatewayBootstrapConfig { key_hash: Some(hash_token("k")), ..base.clone() },
        GatewayBootstrapConfig { key: Some("  ".into()), ..base.clone() },
        GatewayBootstrapConfig { user: "".into(), ..base.clone() },
        GatewayBootstrapConfig { key: None, key_hash: Some("not-a-hash".into()), ..base.clone() },
        GatewayBootstrapConfig { key: None, key_hash: Some(hash_token("k").to_uppercase()), ..base.clone() },
    ];
    for cfg in cases {
        let err = cfg.resolved_hash().unwrap_err().to_string();
        assert!(err.contains("gateway.bootstrap") && err.contains("Refusing to start"), "{err}");
    }
}

#[test]
fn debug_output_never_contains_the_raw_key() {
    let shown = format!("{:?}", with_key("svc", "mr-super-secret-value"));
    assert!(!shown.contains("mr-super-secret-value"), "{shown}");
    assert!(shown.contains("<redacted>"));
}

#[test]
fn serialization_never_contains_the_raw_key() {
    let json = serde_json::to_string(&with_key("svc", "mr-super-secret-value")).unwrap();
    assert!(!json.contains("mr-super-secret-value"), "{json}");
}

#[test]
#[serial_test::serial]
fn bootstrap_parses_from_env() {
    std::env::set_var("MODELROUTER_GATEWAY__BOOTSTRAP__USER", "svc");
    std::env::set_var("MODELROUTER_GATEWAY__BOOTSTRAP__KEY", "mr-from-env");
    let settings = modelrouter::config::load(Some("/nonexistent/path.toml".into()));
    std::env::remove_var("MODELROUTER_GATEWAY__BOOTSTRAP__USER");
    std::env::remove_var("MODELROUTER_GATEWAY__BOOTSTRAP__KEY");
    let b = settings.unwrap().gateway.bootstrap.expect("bootstrap from env");
    assert_eq!(b.user, "svc");
    assert_eq!(b.key.as_deref(), Some("mr-from-env"));
    assert_eq!(b.label, "bootstrap");
}
