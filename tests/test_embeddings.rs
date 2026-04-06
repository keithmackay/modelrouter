mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, Settings};
use modelrouter::db::models::NewUser;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry,
    registry::ProviderRegistry,
};
use modelrouter::router::{
    cache::ResponseCache,
    complexity::ComplexityRouter,
    cost::CostCalculator,
    engine::RequestRouter,
    fallback::FallbackChain,
    policy::PolicyEngine,
};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> TestServer {
    let db = common::in_memory_db().await;
    db.create(NewUser {
        name: "test-user".to_string(),
        api_key_hash: hash_token("test-token"),
        group_name: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "hello".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(ResponseCache::new(&CacheConfig::default()));
    let embedding_registry = Arc::new(EmbeddingRegistry::new_with_mock(
        common::MockEmbeddingAdapter {
            embedding: vec![0.1_f32, 0.2, 0.3],
        },
    ));

    let state = AppState {
        settings: settings.clone(),
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry,
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
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn embeddings_unauthenticated_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "hello world"
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn embeddings_string_input_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "hello world"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["object"], "list");
    assert!(body["data"].is_array());
    assert_eq!(body["data"][0]["object"], "embedding");
    assert!(body["data"][0]["embedding"].is_array());
}

#[tokio::test]
async fn embeddings_array_input_returns_one_entry_per_string() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": ["hello", "world"]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][0]["index"], 0);
    assert_eq!(body["data"][1]["index"], 1);
}

#[tokio::test]
async fn embeddings_invalid_input_type_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": 42
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}
