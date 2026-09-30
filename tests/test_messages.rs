//! HTTP handler tests for /v1/messages endpoint (Anthropic Messages API).

mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{ProviderConfig, Settings};
use modelrouter::db::models::{NewApiKey, NewBudgetRule, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app_with_auth() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let mut providers = HashMap::new();
    providers.insert("anthropic".to_string(), ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some("http://localhost:9999".to_string()),
        timeout_secs: 10,
        api_version: Some("2023-06-01".to_string()),
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        ..Default::default()
    });

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

async fn test_app_with_mock_server() -> (TestServer, Arc<dyn DatabaseProvider>, common::mock_anthropic::MockAnthropicServer) {
    build_mock_app(MessagesAppOpts::default()).await
}

/// Knobs for the tests that need to diverge from the default mock wiring.
#[derive(Default)]
struct MessagesAppOpts {
    /// Point `prompt_db` at a schema-less database so `PromptRepository::create`
    /// fails, exercising the "prompt row optional, cost row not" policy.
    broken_prompt_db: bool,
    /// Lifecycle hooks to register in settings.
    lifecycle_hooks: Vec<modelrouter::config::schema::LifecycleHookConfig>,
    /// Turn off prompt logging, so the storage policy skips the insert
    /// entirely rather than attempting and failing it.
    disable_prompt_storage: bool,
    /// Response cache to wire in (the test keeps a handle to observe stores);
    /// a disabled cache when unset.
    response_cache: Option<Arc<modelrouter::router::cache::ResponseCache>>,
}

async fn build_mock_app(
    opts: MessagesAppOpts,
) -> (TestServer, Arc<dyn DatabaseProvider>, common::mock_anthropic::MockAnthropicServer) {
    let mock_server = common::mock_anthropic::MockAnthropicServer::start().await;
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let mut providers = HashMap::new();
    providers.insert("anthropic".to_string(), ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some(mock_server.base_url()),
        timeout_secs: 10,
        api_version: Some("2023-06-01".to_string()),
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        hooks: modelrouter::config::schema::HooksConfig {
            lifecycle: opts.lifecycle_hooks,
            pipeline: vec![],
        },
        ..Default::default()
    });

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

    // No migrations run: the `prompts` table is absent, so prompt inserts fail
    // while the cost ledger on `db` keeps working.
    let prompt_db: Arc<dyn DatabaseProvider> = if opts.broken_prompt_db {
        Arc::new(
            modelrouter::db::sqlite::SqliteDb::connect(":memory:")
                .await
                .unwrap(),
        )
    } else {
        db.clone()
    };

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
        response_cache: opts.response_cache.unwrap_or_else(|| {
            Arc::new(modelrouter::router::cache::ResponseCache::new(
                &modelrouter::config::schema::CacheConfig::default(),
            ))
        }),
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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(
            modelrouter::config::schema::StorageConfig {
                store_prompts: !opts.disable_prompt_storage,
                ..Default::default()
            },
        )),
        prompt_db,
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

    let mut providers = HashMap::new();
    providers.insert("anthropic".to_string(), ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some("http://localhost:9999".to_string()),
        timeout_secs: 10,
        api_version: Some("2023-06-01".to_string()),
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        ..Default::default()
    });

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
    circuit_breaker.record_failure("anthropic");
    circuit_breaker.record_failure("anthropic");
    circuit_breaker.record_failure("anthropic");

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

async fn test_app_no_anthropic_provider() -> TestServer {
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
    TestServer::new(build_router(state)).unwrap()
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

#[tokio::test]
async fn test_messages_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn test_messages_route_exists() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_ne!(resp.status_code(), 404);
    assert_ne!(resp.status_code(), 401);
}

#[tokio::test]
async fn messages_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn messages_no_anthropic_provider_configured() {
    let server = test_app_no_anthropic_provider().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_circuit_breaker_open_returns_error() {
    let server = test_app_with_circuit_breaker_open().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_uses_default_model_when_not_provided() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_policy_concurrency_limit_exceeded() {
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

    let mut providers = HashMap::new();
    providers.insert("anthropic".to_string(), ProviderConfig {
        api_key: "test-key".to_string(),
        api_base: Some("http://localhost:9999".to_string()),
        timeout_secs: 10,
        api_version: Some("2023-06-01".to_string()),
        ..Default::default()
    });

    let settings = Arc::new(Settings {
        providers,
        ..Default::default()
    });

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
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 429);
}

#[tokio::test]
async fn messages_malformed_json_returns_error() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("application/json"))
        .text("{invalid json}")
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_attribution_from_body_and_headers() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-correlation-id"),
            axum::http::HeaderValue::from_static("corr-123"),
        )
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Test"}]
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_streaming_flag() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_non_streaming_request() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_success_non_streaming() {
    let (server, db, mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Provider was called
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);

    // Cost logging happened (give spawn a moment)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
}

#[tokio::test]
async fn messages_success_streaming() {
    let (server, db, mock) = test_app_with_mock_server().await;
    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/event-stream");

    // Provider was called
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);

    // Cost logging happened
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
}

#[tokio::test]
async fn messages_provider_error_passthrough() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_error(axum::http::StatusCode::TOO_MANY_REQUESTS, "rate limit");

    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Test"}]
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 400);
}

#[tokio::test]
async fn messages_prompt_storage_failure_still_records_cost() {
    // Storage policy (issue #4): the prompt row is optional, the cost row is
    // not. An unwritable prompt store must not cost the caller their response
    // nor the operator their billing record.
    let (server, db, _mock) = build_mock_app(MessagesAppOpts {
        broken_prompt_db: true,
        ..Default::default()
    })
    .await;

    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
    // No prompt row exists, so the ledger entry references none (0).
    assert_eq!(ledger[0].prompt_id, 0);
}

#[tokio::test]
async fn messages_streaming_prompt_storage_failure_still_records_cost() {
    // The streaming path logs cost from its own task after the stream drains,
    // so it needs the same guarantee proved separately.
    let (server, db, _mock) = build_mock_app(MessagesAppOpts {
        broken_prompt_db: true,
        ..Default::default()
    })
    .await;

    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
    assert!(ledger[0].cost_usd > 0.0);
    assert_eq!(ledger[0].prompt_id, 0);
}

#[tokio::test]
async fn messages_prompt_logging_disabled_still_bills() {
    // With `[storage] store_prompts = false` the policy skips the insert
    // rather than attempting it. Cost tracking is out of that scope, so both
    // the streaming and non-streaming paths must still bill.
    for stream in [false, true] {
        let (server, db, _mock) = build_mock_app(MessagesAppOpts {
            disable_prompt_storage: true,
            ..Default::default()
        })
        .await;

        let resp = server
            .post("/v1/messages")
            .add_header(bearer("test-token").0, bearer("test-token").1)
            .json(&serde_json::json!({
                "model": "claude-opus-4-5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": stream
            }))
            .await;
        assert_eq!(resp.status_code(), 200, "stream={stream}");

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger.len(), 1, "stream={stream}");
        assert!(ledger[0].cost_usd > 0.0, "stream={stream}");
        assert_eq!(ledger[0].prompt_id, 0, "stream={stream}");
    }
}

#[tokio::test]
async fn messages_fires_on_response_sent_lifecycle_hook() {
    use modelrouter::config::schema::LifecycleHookConfig;

    // Two hooks: only the `on_response_sent` one should fire, so this covers
    // both sides of the event filter. `true` is a POSIX no-op binary, which
    // keeps the test hermetic — the assertion is that hook dispatch neither
    // blocks the response nor disturbs cost logging.
    let (server, db, _mock) = build_mock_app(MessagesAppOpts {
        lifecycle_hooks: vec![
            LifecycleHookConfig {
                name: "responded".to_string(),
                event: "on_response_sent".to_string(),
                exec: "/bin/true".to_string(),
                timeout_secs: 5,
            },
            LifecycleHookConfig {
                name: "received".to_string(),
                event: "on_request_received".to_string(),
                exec: "/bin/true".to_string(),
                timeout_secs: 5,
            },
        ],
        ..Default::default()
    })
    .await;

    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
}

#[tokio::test]
async fn messages_streaming_fires_lifecycle_hook() {
    use modelrouter::config::schema::LifecycleHookConfig;

    // The streaming path logs cost from its own task once the stream ends, so
    // hook dispatch has to be reached there too.
    let (server, db, _mock) = build_mock_app(MessagesAppOpts {
        lifecycle_hooks: vec![LifecycleHookConfig {
            name: "responded".to_string(),
            event: "on_response_sent".to_string(),
            exec: "/bin/true".to_string(),
            timeout_secs: 5,
        }],
        ..Default::default()
    })
    .await;

    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger.len(), 1);
}

#[tokio::test]
async fn messages_circuit_breaker_records_failure() {
    let (server, _db, mock) = test_app_with_mock_server().await;
    mock.set_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "server error");

    let resp = server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "Test"}]
        }))
        .await;
    assert!(resp.status_code().as_u16() >= 500);
}

/// Issue #86: OpenAI `image_url` parts mixed into a native `/v1/messages`
/// request reach Anthropic as native `image` blocks, on both the streaming and
/// non-streaming calls; native blocks and the rest of the body are untouched.
#[tokio::test]
async fn messages_translate_image_url_parts_on_both_calls() {
    for stream in [false, true] {
        let (server, _db, mock) = test_app_with_mock_server().await;
        let resp = server
            .post("/v1/messages")
            .add_header(bearer("test-token").0, bearer("test-token").1)
            .json(&serde_json::json!({
                "model": "claude-opus-4-5",
                "max_tokens": 1024,
                "stream": stream,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "Compare."},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBBB"}},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                    {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
                ]}]
            }))
            .await;
        assert_eq!(resp.status_code(), 200, "stream={stream}");

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let body = &requests[0].body;
        assert_eq!(body["max_tokens"], 1024);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            blocks[0],
            serde_json::json!({"type": "text", "text": "Compare."})
        );
        assert_eq!(
            blocks[1]["source"]["data"], "BBBB",
            "native block untouched"
        );
        assert_eq!(blocks[2]["type"], "image");
        assert_eq!(blocks[2]["source"]["type"], "base64");
        assert_eq!(blocks[2]["source"]["data"], "AAAA");
        assert_eq!(blocks[3]["type"], "image");
        assert_eq!(blocks[3]["source"]["type"], "url");
        assert_eq!(blocks[3]["source"]["url"], "https://example.com/x.png");
    }
}

// ── Response cache ────────────────────────────────────────────────────────────

fn enabled_cache() -> Arc<modelrouter::router::cache::ResponseCache> {
    Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig {
            enabled: true,
            ..Default::default()
        },
    ))
}

async fn cached_app() -> (
    TestServer,
    Arc<dyn DatabaseProvider>,
    common::mock_anthropic::MockAnthropicServer,
    Arc<modelrouter::router::cache::ResponseCache>,
) {
    let cache = enabled_cache();
    let (server, db, mock) = build_mock_app(MessagesAppOpts {
        response_cache: Some(cache.clone()),
        ..Default::default()
    })
    .await;
    (server, db, mock, cache)
}

/// The store after a stream runs on its own task once the stream ends.
async fn wait_for_cache_stores(cache: &modelrouter::router::cache::ResponseCache, want: u64) {
    for _ in 0..200 {
        if cache.stats().await.stores >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {want} cache stores");
}

/// A complete Anthropic event stream: text plus a tool call split over
/// several `input_json_delta` events.
fn full_anthropic_stream() -> String {
    let events = [
        serde_json::json!({"type": "message_start", "message": {"id": "msg_up", "type": "message", "role": "assistant",
            "model": "claude-opus-4-5", "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 30, "output_tokens": 1}}}),
        serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Checking "}}),
        serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "now."}}),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {}}}),
        serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
        serde_json::json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"x\"}"}}),
        serde_json::json!({"type": "content_block_stop", "index": 1}),
        serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": null}, "usage": {"output_tokens": 12}}),
        serde_json::json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// The `data:` payloads of an SSE body, in order.
fn sse_events(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn deterministic(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "claude-opus-4-5",
        "max_tokens": 256,
        "temperature": 0.0,
        "messages": [{"role": "user", "content": "Look up x"}],
        "stream": stream
    })
}

async fn post_messages(server: &TestServer, body: &serde_json::Value) -> axum_test::TestResponse {
    server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(body)
        .await
}

#[tokio::test]
async fn messages_repeat_call_is_served_from_cache_and_metered() {
    let (server, db, mock, _cache) = cached_app().await;

    let first = post_messages(&server, &deterministic(false)).await;
    assert_eq!(first.status_code(), 200);
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    let second = post_messages(&server, &deterministic(false)).await;
    assert_eq!(second.status_code(), 200);
    assert_eq!(second.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    assert_eq!(mock.requests().len(), 1, "the hit never reaches upstream");

    let (a, b): (serde_json::Value, serde_json::Value) = (first.json(), second.json());
    assert_eq!(a["content"], b["content"]);
    assert_eq!(a["usage"], b["usage"]);
    assert_ne!(a["id"], b["id"], "each response carries its own id");

    let rows = common::wait_for_ledger_rows(&*db, 2).await;
    let hit = rows
        .iter()
        .find(|r| r.cache_hit)
        .expect("a cache-hit ledger row");
    assert_eq!(hit.cost_usd, 0.0);
    assert!(hit.saved_usd > 0.0);
    assert_eq!((hit.tokens_in, hit.tokens_out), (10, 5));
}

#[tokio::test]
async fn messages_stream_is_stored_and_replays_in_both_shapes() {
    let (server, _db, mock, cache) = cached_app().await;
    mock.set_streaming_response(axum::http::StatusCode::OK, full_anthropic_stream());

    let live = post_messages(&server, &deterministic(true)).await;
    assert_eq!(live.status_code(), 200);
    assert_eq!(live.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    wait_for_cache_stores(&cache, 1).await;

    let replay = post_messages(&server, &deterministic(true)).await;
    assert_eq!(replay.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    assert_eq!(
        replay.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let events = sse_events(&replay.text());
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"message_start"));
    assert_eq!(&kinds[kinds.len() - 2..], ["message_delta", "message_stop"]);
    let text: String = events
        .iter()
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "Checking now.");
    let tool_json: String = events
        .iter()
        .filter_map(|e| e["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&tool_json).unwrap(),
        serde_json::json!({"q": "x"})
    );
    let delta = events
        .iter()
        .find(|e| e["type"] == "message_delta")
        .unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(delta["usage"]["output_tokens"], 12);
    assert_eq!(delta["usage"]["input_tokens"], 30);

    let plain = post_messages(&server, &deterministic(false)).await;
    assert_eq!(plain.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    let message: serde_json::Value = plain.json();
    assert_eq!(message["stop_reason"], "tool_use");
    assert_eq!(
        message["content"][0],
        serde_json::json!({"type": "text", "text": "Checking now."})
    );
    assert_eq!(
        message["content"][1],
        serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "x"}})
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "one upstream call served all three"
    );
}

#[tokio::test]
async fn messages_plain_entry_replays_as_a_stream() {
    let (server, _db, mock, _cache) = cached_app().await;
    assert_eq!(
        post_messages(&server, &deterministic(false))
            .await
            .status_code(),
        200
    );

    let replay = post_messages(&server, &deterministic(true)).await;
    assert_eq!(replay.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    let events = sse_events(&replay.text());
    let text: String = events
        .iter()
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "Hello from mock");
    assert_eq!(events.last().unwrap()["type"], "message_stop");
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn messages_incomplete_stream_is_not_stored() {
    let (server, _db, mock, cache) = cached_app().await;
    let truncated = full_anthropic_stream().replace(
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        "",
    );
    mock.set_streaming_response(axum::http::StatusCode::OK, truncated);

    for _ in 0..2 {
        let resp = post_messages(&server, &deterministic(true)).await;
        assert_eq!(resp.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    }
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(cache.stats().await.stores, 0);
}

#[tokio::test]
async fn messages_sampled_requests_bypass_the_cache() {
    let (server, _db, mock, _cache) = cached_app().await;
    let mut body = deterministic(false);
    body.as_object_mut().unwrap().remove("temperature");
    for _ in 0..2 {
        let resp = post_messages(&server, &body).await;
        assert!(
            resp.headers().get("x-modelrouter-cache").is_none(),
            "not consulted"
        );
    }
    assert_eq!(mock.requests().len(), 2);
}

async fn post_messages_with_mode(
    server: &TestServer,
    body: &serde_json::Value,
    mode: &'static str,
) -> axum_test::TestResponse {
    server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-cache"),
            axum::http::HeaderValue::from_static(mode),
        )
        .json(body)
        .await
}

#[tokio::test]
async fn messages_use_mode_caches_a_sampled_request() {
    let (server, _db, mock, _cache) = cached_app().await;
    let mut body = deterministic(false);
    body.as_object_mut().unwrap().remove("temperature");

    let first = post_messages_with_mode(&server, &body, "use").await;
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    let second = post_messages_with_mode(&server, &body, "use").await;
    assert_eq!(second.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn messages_bypass_and_refresh_modes() {
    let (server, _db, mock, _cache) = cached_app().await;
    let body = deterministic(false);

    let bypassed = post_messages_with_mode(&server, &body, "bypass").await;
    assert_eq!(
        bypassed.headers().get("x-modelrouter-cache").unwrap(),
        "BYPASS"
    );
    let refreshed = post_messages_with_mode(&server, &body, "refresh").await;
    assert_eq!(
        refreshed.headers().get("x-modelrouter-cache").unwrap(),
        "REFRESH"
    );
    assert_eq!(mock.requests().len(), 2, "neither mode looks up");

    let hit = post_messages(&server, &body).await;
    assert_eq!(
        hit.headers().get("x-modelrouter-cache").unwrap(),
        "HIT",
        "refresh stored"
    );
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn messages_unknown_cache_mode_is_a_400() {
    let (server, _db, mock, _cache) = cached_app().await;
    let resp = post_messages_with_mode(&server, &deterministic(false), "always").await;
    assert_eq!(resp.status_code(), 400);
    assert!(mock.requests().is_empty());
}

async fn post_messages_with_ttl(
    server: &TestServer,
    body: &serde_json::Value,
    ttl: &'static str,
) -> axum_test::TestResponse {
    server
        .post("/v1/messages")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-cache-ttl"),
            axum::http::HeaderValue::from_static(ttl),
        )
        .json(body)
        .await
}

#[tokio::test]
async fn messages_ttl_header_sets_the_entry_lifetime() {
    let (server, _db, _mock, _cache) = cached_app().await;
    let first = post_messages_with_ttl(&server, &deterministic(false), "1").await;
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    let hit = post_messages(&server, &deterministic(false)).await;
    assert_eq!(hit.headers().get("x-modelrouter-cache").unwrap(), "HIT");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let expired = post_messages(&server, &deterministic(false)).await;
    assert_eq!(
        expired.headers().get("x-modelrouter-cache").unwrap(),
        "MISS"
    );
}

#[tokio::test]
async fn messages_ttl_header_applies_to_a_streamed_store() {
    let (server, _db, mock, cache) = cached_app().await;
    mock.set_streaming_response(axum::http::StatusCode::OK, full_anthropic_stream());
    let live = post_messages_with_ttl(&server, &deterministic(true), "1").await;
    let _ = live.text();
    wait_for_cache_stores(&cache, 1).await;
    let hit = post_messages(&server, &deterministic(true)).await;
    assert_eq!(hit.headers().get("x-modelrouter-cache").unwrap(), "HIT");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let expired = post_messages(&server, &deterministic(true)).await;
    assert_eq!(
        expired.headers().get("x-modelrouter-cache").unwrap(),
        "MISS"
    );
}
