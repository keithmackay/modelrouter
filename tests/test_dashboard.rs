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
    build_test_server_with_db(Arc::new(db), settings).await
}

/// Same wiring as `build_test_server`, but over a caller-supplied database so a
/// test can seed rows before the server reads them.
async fn build_test_server_with_db(
    db: Arc<modelrouter::db::sqlite::SqliteDb>,
    settings: Arc<Settings>,
) -> TestServer {
    let db: Arc<dyn DatabaseProvider> = db;
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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
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

/// The Failures page must render the captured failures, grouped by stage.
///
/// Capturing failures in the database is only half the job: until this page
/// existed the dashboard could answer "what ran" but not "what failed", and an
/// operator's only recourse was the calling application's own logs — which is
/// exactly the dead end that left 196 "Unknown provider" errors undiagnosed for a
/// full run.
#[tokio::test]
async fn failures_page_lists_captured_failures_by_stage() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "anthropic/claude-sonnet-4".to_string(),
            routed_model: Some("claude-sonnet-4".to_string()),
            provider: Some("anthropic".to_string()),
            stage: FailureStage::Resolve,
            status_code: Some(502),
            error_message: "provider error: Unknown provider: anthropic".to_string(),
            attempts: 1,
            latency_ms: Some(3),
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
        },
    )
    .await
    .expect("failure should persist");

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "failures page should render for an admin");
    let body = resp.text();
    // The template HTML-escapes, so the slash arrives as &#x2f; — assert on the
    // distinctive part of the name rather than the raw literal.
    assert!(
        body.contains("claude-sonnet-4"),
        "the failing model must be shown: {body}"
    );
    assert!(
        body.contains("Unknown provider: anthropic"),
        "the provider's own message must be shown verbatim"
    );
    assert!(
        body.contains("resolve"),
        "the stage must be shown so the operator knows it is a config fault"
    );
}

#[tokio::test]
async fn failure_detail_route_returns_200_for_existing() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;
    let created = FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "anthropic/claude-sonnet-4".to_string(),
            routed_model: Some("claude-sonnet-4".to_string()),
            provider: Some("anthropic".to_string()),
            stage: FailureStage::Provider,
            status_code: Some(429),
            error_message: "rate_limit_error: Rate limit exceeded".to_string(),
            attempts: 2,
            latency_ms: Some(123),
            project: None,
            attribution_correlation_id: Some("test-correlation-123".to_string()),
            attribution_tags: "{}".to_string(),
        },
    )
    .await
    .expect("failure should persist");

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get(&format!("/admin/failures/{}", created.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "failure detail should render for an existing failure");
    let body = resp.text();
    assert!(
        body.contains("claude-sonnet-4"),
        "detail must show routed model"
    );
    assert!(
        body.contains("rate_limit_error"),
        "detail must show error message"
    );
    assert!(
        body.contains("test-correlation-123"),
        "detail must show correlation id"
    );
    assert!(
        body.contains("429"),
        "detail must show status code"
    );
}

#[tokio::test]
async fn failure_detail_route_returns_404_for_missing() {
    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures/99999")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "route returns 200 even when not found");
    let body = resp.text();
    assert!(
        body.contains("not found"),
        "body should indicate failure not found"
    );
}

#[tokio::test]
async fn failures_list_filters_by_correlation_id() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;

    // Create failure with correlation id "match-this"
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "model-a".to_string(),
            routed_model: None,
            provider: None,
            stage: FailureStage::Resolve,
            status_code: None,
            error_message: "First failure".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some("match-this".to_string()),
            attribution_tags: "{}".to_string(),
        },
    )
    .await
    .expect("first failure should persist");

    // Create failure with different correlation id
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "model-b".to_string(),
            routed_model: None,
            provider: None,
            stage: FailureStage::Provider,
            status_code: None,
            error_message: "Second failure".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some("different-id".to_string()),
            attribution_tags: "{}".to_string(),
        },
    )
    .await
    .expect("second failure should persist");

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures")
        .add_query_param("correlation_id", "match-this")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "filtered failures page should render");
    let body = resp.text();
    assert!(
        body.contains("First failure"),
        "response must include the matching failure"
    );
    assert!(
        !body.contains("Second failure"),
        "response must NOT include non-matching failure"
    );
}

#[tokio::test]
async fn failures_list_shows_correlation_id_column() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "test-model".to_string(),
            routed_model: None,
            provider: None,
            stage: FailureStage::Resolve,
            status_code: None,
            error_message: "Test error".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some("visible-correlation-id".to_string()),
            attribution_tags: "{}".to_string(),
        },
    )
    .await
    .expect("failure should persist");

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "failures list should render");
    let body = resp.text();
    assert!(
        body.contains("visible-correlation-id"),
        "correlation id must appear in the list"
    );
    assert!(
        body.contains("Correlation ID"),
        "table header for correlation id must be present"
    );
}
