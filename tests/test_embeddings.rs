mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, Settings};
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
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
        email: None,
    })
    .await
    .unwrap();

    let user = UserRepository::find_by_name(&db, "test-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(&db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("test-token"),
        label: Some("test".to_string()),
        expires_at: None,
        project: None,
        session_window_secs: None,
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

/// `encoding_format: "base64"` must return a base64 STRING, not a float array.
///
/// The official OpenAI SDKs send this by default when the caller does not specify
/// a format, and then decode the response as base64. Returning a float array to
/// such a client does not error — the JS SDK runs `Buffer.from(array, 'base64')`
/// over it and produces a shorter vector of zeros. Observed against this router
/// before the fix: a 768-dimension request arrived at the client as 192 zeros,
/// silently, and would have been stored and compared against as if it were real.
#[tokio::test]
async fn embeddings_base64_format_returns_a_string_not_an_array() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "hello world",
            "encoding_format": "base64"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    let embedding = &body["data"][0]["embedding"];
    assert!(
        embedding.is_string(),
        "a base64 request must return a string, got: {embedding}"
    );

    // And it must decode back to the little-endian f32s the adapter produced.
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(embedding.as_str().unwrap())
        .expect("must be valid base64");
    assert_eq!(raw.len() % 4, 0, "decoded bytes must be whole f32s");
    let first = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    assert!((first - 0.1_f32).abs() < 1e-6, "first value should round-trip, got {first}");
}

/// The explicit float format, and the absent-field default, both stay arrays.
#[tokio::test]
async fn embeddings_float_format_stays_an_array() {
    let server = test_app().await;
    for body_json in [
        serde_json::json!({"model":"text-embedding-3-small","input":"x","encoding_format":"float"}),
        serde_json::json!({"model":"text-embedding-3-small","input":"x"}),
    ] {
        let resp = server
            .post("/v1/embeddings")
            .add_header(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer test-token"),
            )
            .json(&body_json)
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert!(
            body["data"][0]["embedding"].is_array(),
            "float format must stay an array"
        );
    }
}

/// An unrecognised format is refused rather than silently treated as float —
/// the whole point is that the caller's decoder and ours must agree.
#[tokio::test]
async fn embeddings_unknown_encoding_format_is_refused() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "x",
            "encoding_format": "float16"
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}
