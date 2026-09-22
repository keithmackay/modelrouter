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
        role: "viewer".to_string(),
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
        assert_eq!(settings.oidc.auto_provision_role, "viewer");
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
            role: "viewer".to_string(),
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
            role: "viewer".to_string(),
        }).await.unwrap();

        // Second insert with same oidc_subject must fail
        let result = db.create_from_oidc(NewAdminUserFromOidc {
            name: "Alice Dup".to_string(),
            email: "alice2@example.com".to_string(),
            oidc_subject: "provider|abc".to_string(),
            role: "viewer".to_string(),
        }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_existing_admin_find_returns_oidc_subject_none() {
        let db = crate::common::in_memory_db().await;
        let created = db.create(NewAdminUser {
            name: "bob".to_string(),
            password_hash: "hash".to_string(),
            role: "viewer".to_string(),
        }).await.unwrap();

        assert!(created.oidc_subject.is_none());
        assert!(created.email.is_none());
    }

    /// Test that OIDC-provisioned admins with `role: "superadmin"` can hold superadmin
    /// (verifies issue #51 fix: the default is now "viewer", but an explicit config
    /// of "superadmin" should work).
    #[tokio::test]
    async fn test_oidc_superadmin_role_yields_superadmin_session() {
        let db = crate::common::in_memory_db().await;

        // Auto-provision with role "superadmin" (operator explicitly configured it)
        let admin = db.create_from_oidc(NewAdminUserFromOidc {
            name: "Super Alice".to_string(),
            email: "alice@example.com".to_string(),
            oidc_subject: "oidc|superadmin-test".to_string(),
            role: "superadmin".to_string(),
        }).await.unwrap();

        assert_eq!(admin.role, "superadmin");
        assert!(admin.enabled);

        // Confirm the DB record is truly "superadmin", not degraded
        let found = db.find_by_oidc_subject("oidc|superadmin-test").await.unwrap().unwrap();
        assert_eq!(found.role, "superadmin");
    }

    /// Test that OIDC-provisioned admins with `role: "viewer"` hold viewer role
    /// (the new fail-safe default).
    #[tokio::test]
    async fn test_oidc_viewer_role_yields_viewer_session() {
        let db = crate::common::in_memory_db().await;

        // Auto-provision with role "viewer" (the new default)
        let admin = db.create_from_oidc(NewAdminUserFromOidc {
            name: "Viewer Bob".to_string(),
            email: "bob@example.com".to_string(),
            oidc_subject: "oidc|viewer-test".to_string(),
            role: "viewer".to_string(),
        }).await.unwrap();

        assert_eq!(admin.role, "viewer");
        assert!(admin.enabled);

        let found = db.find_by_oidc_subject("oidc|viewer-test").await.unwrap().unwrap();
        assert_eq!(found.role, "viewer");
    }

    /// Test OIDC routes when OIDC is disabled
    #[tokio::test]
    async fn test_oidc_login_disabled_returns_404() {
        use modelrouter::config::Settings;
        use modelrouter::api::app::{AppState, DatabaseProvider, build_router};
        use modelrouter::providers::registry::ProviderRegistry;
        use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
        use axum_test::TestServer;
        use std::sync::Arc;
        use std::collections::HashMap;

        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = Settings::default();
        settings.oidc.enabled = false;
        let settings = Arc::new(settings);

        let db_provider: Arc<dyn DatabaseProvider> = db.clone();
        let registry = Arc::new(ProviderRegistry::new_with_mock(crate::common::MockAdapter {
            response: "ok".to_string(),
        }));
        let router = Arc::new(RequestRouter::new(settings.clone()));
        let cost_calc = Arc::new(CostCalculator::new());
        let policy = Arc::new(PolicyEngine::new(db_provider.clone()));
        let fallback = Arc::new(FallbackChain::new(HashMap::new()));
        let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
        let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default()
        ));
        let embedding_registry = Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                crate::common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            )
        );

        let state = AppState {
            live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
            storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
            prompt_db: db_provider.clone(),
            settings,
            db: db_provider.clone(),
            pool: None,
            router,
            cost_calc,
            provider_registry: registry,
            policy,
            fallback,
            complexity_router,
            response_cache,
            embedding_registry,
            search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
            load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
                std::collections::HashMap::new(),
            )),
            concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
            circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
            ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
            session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
            session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
            app_metrics: None,
            callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
            guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
            oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
            experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
        };

        let server = TestServer::new(build_router(state)).unwrap();
        let resp = server.get("/admin/auth/oidc/login").await;
        assert_eq!(resp.status_code(), 404, "OIDC login should return 404 when disabled");
    }

    /// Test OIDC callback rejects invalid state
    #[tokio::test]
    async fn test_oidc_callback_invalid_state() {
        use modelrouter::config::Settings;
        use modelrouter::api::app::{AppState, DatabaseProvider, build_router};
        use modelrouter::providers::registry::ProviderRegistry;
        use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
        use axum_test::TestServer;
        use std::sync::Arc;
        use std::collections::HashMap;

        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = Settings::default();
        settings.oidc.enabled = true;
        settings.oidc.issuer_url = "http://localhost:9999".to_string();
        let settings = Arc::new(settings);

        let db_provider: Arc<dyn DatabaseProvider> = db.clone();
        let registry = Arc::new(ProviderRegistry::new_with_mock(crate::common::MockAdapter {
            response: "ok".to_string(),
        }));
        let router = Arc::new(RequestRouter::new(settings.clone()));
        let cost_calc = Arc::new(CostCalculator::new());
        let policy = Arc::new(PolicyEngine::new(db_provider.clone()));
        let fallback = Arc::new(FallbackChain::new(HashMap::new()));
        let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
        let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default()
        ));
        let embedding_registry = Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                crate::common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            )
        );

        let state = AppState {
            live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
            storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
            prompt_db: db_provider.clone(),
            settings,
            db: db_provider.clone(),
            pool: None,
            router,
            cost_calc,
            provider_registry: registry,
            policy,
            fallback,
            complexity_router,
            response_cache,
            embedding_registry,
            search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
            load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
                std::collections::HashMap::new(),
            )),
            concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
            circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
            ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
            session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
            session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
            app_metrics: None,
            callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
            guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
            oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
            experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
        };

        let server = TestServer::new(build_router(state)).unwrap();
        let resp = server
            .get("/admin/auth/oidc/callback")
            .add_query_param("code", "test-code")
            .add_query_param("state", "invalid-state")
            .await;

        assert_eq!(resp.status_code(), 400, "invalid state should return 400");
    }

    /// Test generate_state creates unique tokens
    #[test]
    fn test_generate_state_unique() {
        use modelrouter::api::admin::oidc::generate_state;
        let state1 = generate_state();
        let state2 = generate_state();
        assert_ne!(state1, state2, "state tokens should be unique");
        assert!(!state1.is_empty());
    }

    /// Test state store expiry logic
    #[test]
    fn test_state_store_take_unknown_returns_none() {
        use modelrouter::api::admin::oidc::OidcStateStore;
        let store = OidcStateStore::new();
        assert!(store.take("unknown").is_none());
    }

    /// Test PKCE challenge with wrong verifier fails
    #[test]
    fn test_pkce_wrong_verifier_fails() {
        use modelrouter::api::admin::oidc::{generate_pkce_pair, verify_pkce_challenge};
        let (_, challenge) = generate_pkce_pair();
        assert!(!verify_pkce_challenge("wrong-verifier", &challenge));
    }

    /// Test email validation with malformed email
    #[test]
    fn test_email_allowed_malformed() {
        use modelrouter::api::admin::oidc::is_email_allowed;
        assert!(
            !is_email_allowed("no-at-sign", &[], &vec!["example.com".to_string()]),
            "malformed email should not match domain"
        );
    }

    /// Test discovery helper handles network errors
    #[tokio::test]
    async fn test_fetch_discovery_network_error() {
        use modelrouter::api::admin::oidc::fetch_discovery;
        let client = reqwest::Client::new();
        let result = fetch_discovery("http://localhost:9999", &client).await;
        assert!(result.is_err(), "unreachable issuer should error");
    }

    /// Test token exchange helper handles network errors
    #[tokio::test]
    async fn test_exchange_code_network_error() {
        use modelrouter::api::admin::oidc::exchange_code;
        let client = reqwest::Client::new();
        let result = exchange_code(
            "http://localhost:9999/token",
            "client",
            "secret",
            "code",
            "verifier",
            "redirect",
            &client,
        ).await;
        assert!(result.is_err());
    }

    /// Test ID token validation with malformed token
    #[tokio::test]
    async fn test_validate_id_token_malformed() {
        use modelrouter::api::admin::oidc::validate_id_token;
        let client = reqwest::Client::new();
        let result = validate_id_token(
            "not.a.jwt",
            "http://localhost:9999/jwks",
            "client",
            "http://localhost:9999",
            &client,
        ).await;
        assert!(result.is_err());
    }
}

/// The real login → callback flow, driven against a mock OIDC issuer served over
/// localhost HTTP (`common::mock_oidc`).
///
/// Everything below exercises production code end to end: `oidc_login` performs
/// a real discovery fetch and builds a real authorization redirect; `oidc_callback`
/// performs a real token exchange, a real JWKS fetch and a real RS256 signature
/// validation before touching the database. The mock is programmable at runtime,
/// so each failure arm is reached by completing a *successful* login (which is the
/// only way to obtain a valid state token) and then breaking exactly one thing
/// about the issuer before the callback.
mod oidc_flow_tests {
    use crate::common::mock_oidc::{
        DiscoveryMode, IdTokenClaimSpec, JwksMode, MockOidc, TokenMode,
    };
    use axum_test::TestServer;
    use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
    use modelrouter::config::Settings;
    use modelrouter::db::models::NewAdminUserFromOidc;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;
    use modelrouter::db::sqlite::SqliteDb;
    use modelrouter::providers::registry::ProviderRegistry;
    use modelrouter::router::{
        cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    const CLIENT_ID: &str = "mock-client-id";
    const REDIRECT_URI: &str = "http://localhost:8080/admin/auth/oidc/callback";

    /// Settings wired to a running mock issuer, with OIDC on.
    fn settings_for(issuer: &MockOidc) -> Settings {
        let mut settings = Settings::default();
        settings.oidc.enabled = true;
        settings.oidc.issuer_url = issuer.issuer_url();
        settings.oidc.client_id = CLIENT_ID.to_string();
        settings.oidc.client_secret = "mock-client-secret".to_string();
        settings.oidc.redirect_uri = REDIRECT_URI.to_string();
        settings
    }

    async fn server_with(db: Arc<SqliteDb>, settings: Settings) -> TestServer {
        let settings = Arc::new(settings);
        let db: Arc<dyn DatabaseProvider> = db;
        let registry = Arc::new(ProviderRegistry::new_with_mock(crate::common::MockAdapter {
            response: "ok".to_string(),
        }));
        let state = AppState {
            live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
            storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
            prompt_db: db.clone(),
            router: Arc::new(RequestRouter::new(settings.clone())),
            cost_calc: Arc::new(CostCalculator::new()),
            provider_registry: registry,
            policy: Arc::new(PolicyEngine::new(db.clone())),
            fallback: Arc::new(FallbackChain::new(HashMap::new())),
            complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
            response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
                &modelrouter::config::schema::CacheConfig::default(),
            )),
            embedding_registry: Arc::new(
                modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                    crate::common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
                ),
            ),
            search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(
                HashMap::new(),
            )),
            load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
                HashMap::new(),
            )),
            concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
            circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
            ip_rate_limiter: Arc::new(
                modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
            ),
            session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
            session_affinity: Arc::new(
                modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
            ),
            app_metrics: None,
            callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
            guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
            oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
            experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
            settings,
            db: db.clone(),
            pool: None,
        };
        TestServer::new(build_router(state)).unwrap()
    }

    /// A running issuer, a server pointed at it, and the database behind it.
    async fn harness() -> (MockOidc, TestServer, Arc<SqliteDb>) {
        let issuer = MockOidc::start().await;
        let db = Arc::new(crate::common::in_memory_db().await);
        let server = server_with(db.clone(), settings_for(&issuer)).await;
        (issuer, server, db)
    }

    /// Perform a real `/admin/auth/oidc/login` and return the `state` token the
    /// handler minted. This is the only way a test can hold a state the server
    /// will accept, which is what makes the callback's later arms reachable.
    async fn login_and_take_state(server: &TestServer) -> String {
        let resp = server.get("/admin/auth/oidc/login").await;
        assert_eq!(
            resp.status_code(),
            307,
            "login should redirect to the provider's authorization endpoint"
        );
        let location = resp
            .headers()
            .get("location")
            .expect("redirect carries a Location")
            .to_str()
            .unwrap()
            .to_string();
        let url = reqwest::Url::parse(&location).expect("Location is an absolute URL");
        url.query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("authorization URL carries a state parameter")
    }

    async fn callback(server: &TestServer, code: &str, state: &str) -> axum_test::TestResponse {
        server
            .get("/admin/auth/oidc/callback")
            .add_query_param("code", code)
            .add_query_param("state", state)
            .await
    }

    // ── login ────────────────────────────────────────────────────────────────

    /// The redirect must carry every parameter the provider needs, including the
    /// S256 challenge — a redirect missing `code_challenge_method` would silently
    /// downgrade PKCE to plain.
    #[tokio::test]
    async fn login_redirects_to_authorization_endpoint_with_pkce() {
        let (issuer, server, _db) = harness().await;

        let resp = server.get("/admin/auth/oidc/login").await;
        assert_eq!(resp.status_code(), 307);

        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert!(
            location.starts_with(&format!("{}/authorize", issuer.issuer_url())),
            "redirect should target the discovered authorization_endpoint, got {location}"
        );

        let url = reqwest::Url::parse(location).unwrap();
        let q: HashMap<String, String> =
            url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(q.get("redirect_uri").map(String::as_str), Some(REDIRECT_URI));
        assert_eq!(q.get("scope").map(String::as_str), Some("openid email profile"));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert!(!q.get("state").expect("state present").is_empty());
        assert!(!q.get("code_challenge").expect("challenge present").is_empty());
    }

    /// Two logins must not reuse a state token; a fixed state would let one
    /// browser's callback consume another's pending login.
    #[tokio::test]
    async fn login_mints_a_fresh_state_each_time() {
        let (_issuer, server, _db) = harness().await;
        let first = login_and_take_state(&server).await;
        let second = login_and_take_state(&server).await;
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn login_returns_502_when_discovery_is_unavailable() {
        let (issuer, server, _db) = harness().await;
        issuer.set_discovery(DiscoveryMode::ServerError);

        let resp = server.get("/admin/auth/oidc/login").await;
        assert_eq!(resp.status_code(), 502);
        assert!(resp.text().contains("OIDC discovery failed"));
    }

    /// Discovery that returns 200 but without the required fields is just as
    /// unusable as a 500, and must not be treated as success.
    #[tokio::test]
    async fn login_returns_502_when_discovery_body_is_malformed() {
        let (issuer, server, _db) = harness().await;
        issuer.set_discovery(DiscoveryMode::Malformed);

        let resp = server.get("/admin/auth/oidc/login").await;
        assert_eq!(resp.status_code(), 502);
    }

    /// A provider advertising a non-URL authorization endpoint must produce a
    /// clean 502 rather than a panic or a redirect to nowhere.
    #[tokio::test]
    async fn login_returns_502_when_authorization_endpoint_is_not_a_url() {
        let (issuer, server, _db) = harness().await;
        issuer.set_discovery(DiscoveryMode::BadAuthorizationEndpoint);

        let resp = server.get("/admin/auth/oidc/login").await;
        assert_eq!(resp.status_code(), 502);
        assert!(resp.text().contains("Invalid authorization_endpoint"));
    }

    // ── callback: the happy path ─────────────────────────────────────────────

    /// The whole flow: login, exchange, validate, auto-provision, session cookie.
    #[tokio::test]
    async fn callback_auto_provisions_the_admin_and_sets_a_session_cookie() {
        let (issuer, server, db) = harness().await;
        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|new-user".to_string(),
            email: Some("newbie@example.com".to_string()),
            name: Some("New Bie".to_string()),
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "the-auth-code", &state).await;

        assert_eq!(resp.status_code(), 303, "successful login redirects to the dashboard");
        assert_eq!(resp.headers().get("location").unwrap(), "/admin");

        let cookie = resp
            .headers()
            .get("set-cookie")
            .expect("a session cookie is set")
            .to_str()
            .unwrap();
        assert!(cookie.starts_with("mr_admin_session="), "got {cookie}");
        assert!(cookie.contains("HttpOnly"), "session cookie must be HttpOnly");
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));

        let admin = db
            .find_by_oidc_subject("mock-oidc|new-user")
            .await
            .unwrap()
            .expect("the admin was auto-provisioned");
        assert_eq!(admin.name, "New Bie");
        assert_eq!(admin.email.as_deref(), Some("newbie@example.com"));
        assert_eq!(admin.role, "viewer", "default auto-provision role is the fail-safe one");
        assert_eq!(admin.password_hash, "", "an OIDC admin has no local password");
        assert!(admin.last_login_at.is_some(), "last login is stamped on sign-in");
    }

    /// The token exchange must actually carry the PKCE verifier and the client
    /// credentials; a provider enforcing PKCE would reject anything less.
    #[tokio::test]
    async fn callback_sends_the_code_verifier_and_client_credentials_upstream() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "the-auth-code", &state).await;
        assert_eq!(resp.status_code(), 303);

        let requests = issuer.token_requests();
        assert_eq!(requests.len(), 1, "exactly one token exchange");
        let body = &requests[0];
        assert!(body.contains("grant_type=authorization_code"), "got {body}");
        assert!(body.contains("code=the-auth-code"), "got {body}");
        assert!(body.contains("client_id=mock-client-id"), "got {body}");
        assert!(body.contains("client_secret=mock-client-secret"), "got {body}");
        assert!(body.contains("code_verifier="), "PKCE verifier must be sent: {body}");
    }

    /// A second sign-in reuses the existing row rather than provisioning a twin.
    #[tokio::test]
    async fn callback_reuses_an_existing_admin_for_a_known_subject() {
        let (issuer, server, db) = harness().await;
        let existing = db
            .create_from_oidc(NewAdminUserFromOidc {
                name: "Already Here".to_string(),
                email: "already@example.com".to_string(),
                oidc_subject: "mock-oidc|returning".to_string(),
                role: "superadmin".to_string(),
            })
            .await
            .unwrap();

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|returning".to_string(),
            email: Some("already@example.com".to_string()),
            name: Some("Renamed In The IdP".to_string()),
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 303);

        let found = db.find_by_oidc_subject("mock-oidc|returning").await.unwrap().unwrap();
        assert_eq!(found.id, existing.id, "no duplicate row was created");
        assert_eq!(
            found.name, "Already Here",
            "the local record is authoritative; a rename in the IdP does not rewrite it"
        );
    }

    /// An ID token with no `name` claim must still provision, falling back to the
    /// email as the display name rather than creating a nameless admin.
    #[tokio::test]
    async fn callback_falls_back_to_the_email_when_the_token_carries_no_name() {
        let (issuer, server, db) = harness().await;
        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|nameless".to_string(),
            email: Some("nameless@example.com".to_string()),
            name: None,
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        assert_eq!(callback(&server, "code", &state).await.status_code(), 303);

        let admin = db.find_by_oidc_subject("mock-oidc|nameless").await.unwrap().unwrap();
        assert_eq!(admin.name, "nameless@example.com");
    }

    /// The configured auto-provision role is what the new admin gets.
    #[tokio::test]
    async fn callback_honours_the_configured_auto_provision_role() {
        let issuer = MockOidc::start().await;
        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = settings_for(&issuer);
        settings.oidc.auto_provision_role = "superadmin".to_string();
        let server = server_with(db.clone(), settings).await;

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|elevated".to_string(),
            ..Default::default()
        });
        let state = login_and_take_state(&server).await;
        assert_eq!(callback(&server, "code", &state).await.status_code(), 303);

        let admin = db.find_by_oidc_subject("mock-oidc|elevated").await.unwrap().unwrap();
        assert_eq!(admin.role, "superadmin");
    }

    /// The cookie the callback sets must be a session the rest of the dashboard
    /// accepts — otherwise a "successful" login lands the user back on /login.
    #[tokio::test]
    async fn the_session_cookie_from_a_callback_authenticates_the_dashboard() {
        let (_issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 303);

        let cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        let value = cookie.split(';').next().unwrap().to_string();

        let overview = server
            .get("/admin")
            .add_header(
                axum::http::HeaderName::from_static("cookie"),
                axum::http::HeaderValue::from_str(&value).unwrap(),
            )
            .await;
        assert_eq!(
            overview.status_code(),
            200,
            "the OIDC session should open the dashboard, not bounce to login"
        );
    }

    // ── callback: state handling ─────────────────────────────────────────────

    /// State is single-use: replaying a callback must not mint a second session.
    #[tokio::test]
    async fn callback_state_is_single_use() {
        let (_issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;

        assert_eq!(callback(&server, "code", &state).await.status_code(), 303);
        let replay = callback(&server, "code", &state).await;
        assert_eq!(replay.status_code(), 400, "a replayed state must be rejected");
        assert!(replay.text().contains("Invalid or expired state"));
    }

    /// Missing query parameters are a malformed request, not a server fault.
    #[tokio::test]
    async fn callback_without_a_code_parameter_is_rejected() {
        let (_issuer, server, _db) = harness().await;
        let resp = server
            .get("/admin/auth/oidc/callback")
            .add_query_param("state", "whatever")
            .await;
        assert!(
            resp.status_code().is_client_error(),
            "a callback with no code must not reach the token exchange"
        );
    }

    /// Turning OIDC off must close the callback too, not just the login route —
    /// otherwise a stale state token from a previous configuration could still be
    /// redeemed for a session.
    #[tokio::test]
    async fn callback_is_404_when_oidc_is_disabled() {
        let issuer = MockOidc::start().await;
        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = settings_for(&issuer);
        settings.oidc.enabled = false;
        let server = server_with(db, settings).await;

        let resp = callback(&server, "code", "any-state").await;
        assert_eq!(resp.status_code(), 404);
        assert!(resp.text().contains("OIDC not enabled"));
    }

    // ── callback: upstream failures ──────────────────────────────────────────

    #[tokio::test]
    async fn callback_returns_502_when_discovery_fails() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_discovery(DiscoveryMode::ServerError);

        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 502);
        assert!(resp.text().contains("OIDC discovery failed"));
    }

    #[tokio::test]
    async fn callback_returns_502_when_the_token_endpoint_rejects_the_code() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_token(TokenMode::BadRequest);

        let resp = callback(&server, "stale-code", &state).await;
        assert_eq!(resp.status_code(), 502);
        assert!(resp.text().contains("Token exchange failed"));
    }

    /// A 200 from the token endpoint that carries no `id_token` is not a login.
    #[tokio::test]
    async fn callback_returns_502_when_the_token_response_has_no_id_token() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_token(TokenMode::MissingIdToken);

        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 502);
    }

    // ── callback: ID token validation ────────────────────────────────────────

    #[tokio::test]
    async fn callback_rejects_an_unparseable_id_token() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_token(TokenMode::Garbage);

        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 401);
        assert!(resp.text().contains("ID token validation failed"));
    }

    /// Without a `kid` the server cannot know which key signed the token, and
    /// must refuse rather than guess at the first key in the JWKS.
    #[tokio::test]
    async fn callback_rejects_an_id_token_with_no_kid_header() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_token(TokenMode::NoKid);

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
    }

    /// A `kid` absent from the JWKS is the shape a key-rotation race takes.
    #[tokio::test]
    async fn callback_rejects_an_id_token_signed_by_an_unknown_kid() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_token(TokenMode::UnknownKid);

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
    }

    #[tokio::test]
    async fn callback_rejects_when_the_jwks_endpoint_is_down() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_jwks(JwksMode::ServerError);

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
    }

    #[tokio::test]
    async fn callback_rejects_a_jwks_document_with_no_keys_array() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_jwks(JwksMode::NoKeysArray);

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
    }

    #[tokio::test]
    async fn callback_rejects_a_jwks_document_with_an_empty_key_set() {
        let (issuer, server, _db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_jwks(JwksMode::EmptyKeys);

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
    }

    /// A correctly-signed token minted for a *different* client must not open a
    /// session here — this is the token-substitution attack the `aud` check exists
    /// to stop, and the signature alone would pass.
    #[tokio::test]
    async fn callback_rejects_an_id_token_issued_for_another_audience() {
        let (issuer, server, db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|wrong-aud".to_string(),
            aud: "some-other-application".to_string(),
            ..Default::default()
        });

        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 401);
        assert!(
            db.find_by_oidc_subject("mock-oidc|wrong-aud").await.unwrap().is_none(),
            "a rejected token must not provision an admin"
        );
    }

    /// Likewise for a token whose `iss` is not the configured issuer.
    #[tokio::test]
    async fn callback_rejects_an_id_token_from_an_unexpected_issuer() {
        let (issuer, server, db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|wrong-iss".to_string(),
            iss: Some("https://issuer.invalid".to_string()),
            ..Default::default()
        });

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
        assert!(db.find_by_oidc_subject("mock-oidc|wrong-iss").await.unwrap().is_none());
    }

    /// An expired token is a replayed one; the `exp` check is the only thing
    /// bounding how long a leaked ID token stays useful.
    #[tokio::test]
    async fn callback_rejects_an_expired_id_token() {
        let (issuer, server, db) = harness().await;
        let state = login_and_take_state(&server).await;
        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|expired".to_string(),
            expires_in_secs: -3600,
            ..Default::default()
        });

        assert_eq!(callback(&server, "code", &state).await.status_code(), 401);
        assert!(db.find_by_oidc_subject("mock-oidc|expired").await.unwrap().is_none());
    }

    // ── callback: authorisation and account state ────────────────────────────

    /// An allow-list that the signed-in email misses is a 403, and — the part
    /// worth asserting — no admin row is created for the rejected identity.
    #[tokio::test]
    async fn callback_refuses_an_email_outside_the_allow_list() {
        let issuer = MockOidc::start().await;
        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = settings_for(&issuer);
        settings.oidc.allowed_domains = vec!["permitted.example".to_string()];
        let server = server_with(db.clone(), settings).await;

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|outsider".to_string(),
            email: Some("eve@not-permitted.example".to_string()),
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 403);
        assert!(resp.text().contains("Email not permitted"));
        assert!(
            db.find_by_oidc_subject("mock-oidc|outsider").await.unwrap().is_none(),
            "a rejected email must not be auto-provisioned"
        );
    }

    /// The same allow-list must admit an address inside it.
    #[tokio::test]
    async fn callback_admits_an_email_inside_the_allow_list() {
        let issuer = MockOidc::start().await;
        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = settings_for(&issuer);
        settings.oidc.allowed_emails = vec!["named@elsewhere.example".to_string()];
        settings.oidc.allowed_domains = vec!["permitted.example".to_string()];
        let server = server_with(db.clone(), settings).await;

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|insider".to_string(),
            email: Some("named@elsewhere.example".to_string()),
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        assert_eq!(callback(&server, "code", &state).await.status_code(), 303);
        assert!(db.find_by_oidc_subject("mock-oidc|insider").await.unwrap().is_some());
    }

    /// A token with no `email` claim collapses to the empty string, which an
    /// allow-list must treat as not-permitted rather than as a wildcard.
    #[tokio::test]
    async fn callback_refuses_a_token_with_no_email_when_an_allow_list_is_set() {
        let issuer = MockOidc::start().await;
        let db = Arc::new(crate::common::in_memory_db().await);
        let mut settings = settings_for(&issuer);
        settings.oidc.allowed_domains = vec!["permitted.example".to_string()];
        let server = server_with(db.clone(), settings).await;

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|anonymous".to_string(),
            email: None,
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        assert_eq!(callback(&server, "code", &state).await.status_code(), 403);
    }

    /// Disabling an admin locally must survive a fresh IdP sign-in — otherwise
    /// off-boarding through the dashboard would be undone by the next login.
    #[tokio::test]
    async fn callback_refuses_a_disabled_admin() {
        let (issuer, server, db) = harness().await;
        let admin = db
            .create_from_oidc(NewAdminUserFromOidc {
                name: "Off Boarded".to_string(),
                email: "off@example.com".to_string(),
                oidc_subject: "mock-oidc|disabled".to_string(),
                role: "viewer".to_string(),
            })
            .await
            .unwrap();
        sqlx::query("UPDATE admin_users SET enabled = 0 WHERE id = ?")
            .bind(admin.id)
            .execute(&db.pool)
            .await
            .expect("the admin can be disabled");

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|disabled".to_string(),
            email: Some("off@example.com".to_string()),
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 403);
        assert!(resp.text().contains("Account disabled"));
        assert!(
            resp.headers().get("set-cookie").is_none(),
            "a refused login must not set a session cookie"
        );
    }

    /// `admin_users.name` is UNIQUE, so an IdP display name colliding with an
    /// existing local admin makes provisioning fail. That must surface as a 500
    /// with no session, not a panic or a half-created account.
    #[tokio::test]
    async fn callback_returns_500_when_provisioning_collides_with_an_existing_admin() {
        use modelrouter::db::models::NewAdminUser;

        let (issuer, server, db) = harness().await;
        db.create(NewAdminUser {
            name: "Taken Name".to_string(),
            password_hash: "hash".to_string(),
            role: "viewer".to_string(),
        })
        .await
        .unwrap();

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|collides".to_string(),
            email: Some("collides@example.com".to_string()),
            name: Some("Taken Name".to_string()),
            ..Default::default()
        });

        let state = login_and_take_state(&server).await;
        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 500);
        assert!(resp.text().contains("Provisioning failed"));
        assert!(resp.headers().get("set-cookie").is_none());
    }

    /// A storage fault during the subject lookup must be an opaque 500, with no
    /// session issued — dropping the table under the live server is the cheapest
    /// way to make that arm actually run.
    #[tokio::test]
    async fn callback_returns_500_when_the_admin_lookup_hits_a_storage_fault() {
        let (issuer, server, db) = harness().await;
        let state = login_and_take_state(&server).await;

        sqlx::query("DROP TABLE admin_users")
            .execute(&db.pool)
            .await
            .expect("admin_users can be dropped");

        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|storage-fault".to_string(),
            ..Default::default()
        });

        let resp = callback(&server, "code", &state).await;
        assert_eq!(resp.status_code(), 500);
        assert!(resp.text().contains("DB error"));
        assert!(resp.headers().get("set-cookie").is_none());
    }

    // ── helpers, driven directly ─────────────────────────────────────────────

    /// `fetch_discovery` must tolerate a trailing slash on the configured issuer;
    /// a doubled `//.well-known` 404s at most real providers.
    #[tokio::test]
    async fn fetch_discovery_trims_a_trailing_slash_on_the_issuer_url() {
        use modelrouter::api::admin::oidc::fetch_discovery;
        let issuer = MockOidc::start().await;
        let client = reqwest::Client::new();

        let with_slash = format!("{}/", issuer.issuer_url());
        let d = fetch_discovery(&with_slash, &client).await.expect("discovery resolves");
        assert!(d.authorization_endpoint.ends_with("/authorize"));
        assert!(d.token_endpoint.ends_with("/token"));
        assert!(d.jwks_uri.ends_with("/jwks"));
    }

    #[tokio::test]
    async fn validate_id_token_accepts_a_correctly_signed_token() {
        use modelrouter::api::admin::oidc::validate_id_token;
        let issuer = MockOidc::start().await;
        issuer.set_claims(IdTokenClaimSpec {
            sub: "mock-oidc|direct".to_string(),
            email: Some("direct@example.com".to_string()),
            name: Some("Direct Call".to_string()),
            ..Default::default()
        });
        let token = issuer.mint_id_token();

        let claims = validate_id_token(
            &token,
            &issuer.jwks_uri(),
            CLIENT_ID,
            &issuer.issuer_url(),
            &reqwest::Client::new(),
        )
        .await
        .expect("a well-formed token validates");

        assert_eq!(claims.sub, "mock-oidc|direct");
        assert_eq!(claims.email.as_deref(), Some("direct@example.com"));
        assert_eq!(claims.name.as_deref(), Some("Direct Call"));
        assert!(claims.exp > 0);
    }

    /// `exchange_code` returns the provider's `id_token` verbatim.
    #[tokio::test]
    async fn exchange_code_returns_the_id_token_from_the_provider() {
        use modelrouter::api::admin::oidc::exchange_code;
        let issuer = MockOidc::start().await;
        let token_endpoint = format!("{}/token", issuer.issuer_url());

        let resp = exchange_code(
            &token_endpoint,
            CLIENT_ID,
            "secret",
            "the-code",
            "the-verifier",
            REDIRECT_URI,
            &reqwest::Client::new(),
        )
        .await
        .expect("the exchange succeeds");

        assert_eq!(resp.id_token.matches('.').count(), 2, "a compact JWS has three parts");
        let sent = issuer.token_requests();
        assert!(sent[0].contains("code_verifier=the-verifier"));
    }
}
