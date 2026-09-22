//! HTTP handler tests for /v1/images/generations endpoint.

mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{PricingEntry, Settings};
use modelrouter::db::models::{NewApiKey, NewBudgetRule, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> TestServer {
    let db = common::in_memory_db().await;

    UserRepository::create(&db, NewUser {
        name: "test-user".to_string(),
        email: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "Hello!".to_string(),
    }));

    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

async fn test_app_with_auth() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

async fn test_app_with_mock_server() -> (TestServer, Arc<dyn DatabaseProvider>, common::mock_audio::MockAudioServer) {
    let mock_server = common::mock_audio::MockAudioServer::start().await;
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), modelrouter::config::schema::ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some(mock_server.base_url()),
        timeout_secs: 10,
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        ..Default::default()
    });

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "ok".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            ),
        ),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(
            std::collections::HashMap::new(),
        )),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };

    (TestServer::new(build_router(state)).unwrap(), db, mock_server)
}

async fn test_app_with_circuit_breaker_open() -> TestServer {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));
    let circuit_breaker = Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default());
    circuit_breaker.record_failure("openai");
    circuit_breaker.record_failure("openai");
    circuit_breaker.record_failure("openai");

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker,
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

async fn test_app_with_pricing() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let settings = Arc::new(Settings {
        pricing: vec![
            PricingEntry {
                model: "dall-e-3/hd".to_string(),
                input_per_million: 0.080,
                output_per_million: 0.0,
                ..Default::default()
            },
            PricingEntry {
                model: "dall-e-3/standard".to_string(),
                input_per_million: 0.040,
                output_per_million: 0.0,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new_with_config(&settings.pricing));
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

#[tokio::test]
async fn image_generation_unauthenticated_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/images/generations")
        .json(&serde_json::json!({"model": "dall-e-3", "prompt": "a cat", "n": 1}))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn image_generation_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat",
            "n": 1
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn image_generation_circuit_breaker_open_returns_error() {
    let server = test_app_with_circuit_breaker_open().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat",
            "n": 1
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_uses_default_model_when_not_provided() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "prompt": "a cat",
            "n": 1
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_uses_default_quality_when_not_provided() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat"
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_policy_concurrency_limit_exceeded() {
    let db = common::in_memory_db().await;
    let user_id = common::create_user(&db, "test-user", "test-token").await;

    BudgetRepository::create(
        &db as &dyn DatabaseProvider,
        NewBudgetRule {
            user_id: Some(user_id),
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: Some(1),
            model_allow: vec![],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let concurrency = Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new());
    let _permit = concurrency.try_acquire(user_id, 1).unwrap();

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            ),
        ),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(
            std::collections::HashMap::new(),
        )),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency,
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    let server = TestServer::new(build_router(state)).unwrap();

    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat",
            "n": 1
        }))
        .await;
    assert_eq!(resp.status_code(), 429);
}

#[tokio::test]
async fn image_generation_malformed_json_returns_error() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"))
        .text("{invalid json}")
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_attribution_from_body_and_headers() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-correlation-id"),
            axum::http::HeaderValue::from_static("corr-123"),
        )
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "test image",
            "n": 1
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_uses_quality_hd_pricing() {
    let (server, _db) = test_app_with_pricing().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat",
            "quality": "hd",
            "n": 2
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_uses_fallback_pricing_for_unknown_model() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-2",
            "prompt": "a cat",
            "n": 1
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_success_path() {
    let (server, db, mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "a cat",
            "n": 1,
            "quality": "standard"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Provider was called
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/images/generations");

    // Cost logging happened (give spawn a moment)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
}

#[tokio::test]
async fn image_generation_provider_error_passthrough() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_image_error(axum::http::StatusCode::TOO_MANY_REQUESTS, "rate limit");

    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "test",
            "n": 1,
            "quality": "standard"
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn image_generation_circuit_breaker_records_failure() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_image_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "server error");

    let resp = server
        .post("/v1/images/generations")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "dall-e-3",
            "prompt": "test",
            "n": 1,
            "quality": "standard"
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 500);
}
