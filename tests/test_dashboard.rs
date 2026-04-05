mod common;

use axum_test::TestServer;
use modelrouter::api::app::{AppState, DatabaseProvider, build_router};
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    cost::CostCalculator,
    engine::RequestRouter,
    fallback::FallbackChain,
    policy::PolicyEngine,
};
use modelrouter::config::Settings;
use std::collections::HashMap;
use std::sync::Arc;

async fn build_test_server() -> TestServer {
    let db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        settings,
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry: registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        app_metrics: None,
    };

    TestServer::new(build_router(state)).unwrap()
}

fn viewer_jwt(settings: &Settings) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    let claims = AdminClaims {
        sub: 999,
        name: "viewer-user".to_string(),
        role: "viewer".to_string(),
        exp,
    };
    issue_jwt(&claims, &settings.auth.jwt_secret).unwrap()
}

// Test 1: Unauthenticated GET /admin → 303 redirect to /admin/login
#[tokio::test]
async fn unauthenticated_redirect() {
    let server = build_test_server().await;
    let resp = server.get("/admin").await;
    assert_eq!(resp.status_code(), 303, "GET /admin without cookie should redirect");
    let location = resp.headers().get("location").expect("should have location header");
    assert_eq!(location.to_str().unwrap(), "/admin/login");
}

// Test 2: GET /admin/login → 200 with HTML form
#[tokio::test]
async fn login_renders_form() {
    let server = build_test_server().await;
    let resp = server.get("/admin/login").await;
    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("<form"), "login page should contain a form");
    assert!(body.contains("password"), "login page should have password field");
}

// Test 3: POST /admin/login with valid credentials → 303 + Set-Cookie
#[tokio::test]
async fn login_success_sets_cookie() {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    // Create an admin user in DB (cost=4 for speed in tests)
    let password = "test-password-123";
    let password_hash = bcrypt::hash(password, 4).unwrap();
    AdminUserRepository::create(
        &raw_db,
        NewAdminUser {
            name: "testadmin".to_string(),
            password_hash,
            role: "superadmin".to_string(),
        },
    )
    .await
    .unwrap();

    let db: Arc<dyn DatabaseProvider> = Arc::new(raw_db);
    let registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        settings,
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry: registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        app_metrics: None,
    };

    let server = TestServer::new(build_router(state)).unwrap();

    let resp = server
        .post("/admin/login")
        .form(&[("username", "testadmin"), ("password", password)])
        .await;

    assert_eq!(resp.status_code(), 303, "successful login should redirect (got {})", resp.status_code());
    let set_cookie = resp.headers().get("set-cookie").expect("should set a cookie");
    let cookie_str = set_cookie.to_str().unwrap();
    assert!(cookie_str.contains("mr_admin_session="), "should set mr_admin_session cookie");
    assert!(cookie_str.to_lowercase().contains("httponly"), "cookie should be HttpOnly");
}

// Test 4: GET /admin/admins with a viewer JWT cookie → 403
#[tokio::test]
async fn superadmin_only_admins_page() {
    let db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        settings,
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry: registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        app_metrics: None,
    };

    let server = TestServer::new(build_router(state)).unwrap();

    let resp = server
        .get("/admin/admins")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 403, "viewer role should get 403 on /admin/admins");
}
