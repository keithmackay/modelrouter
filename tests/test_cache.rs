mod common;

use modelrouter::router::cache::{make_cache_key, ResponseCache};
use modelrouter::config::schema::CacheConfig;
use modelrouter::providers::adapter::CompletionResult;
use serde_json::json;

fn enabled_cache(max_entries: u64, ttl_seconds: u64) -> ResponseCache {
    ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries,
        ttl_seconds,
    })
}

#[tokio::test]
async fn cache_miss_returns_none() {
    let cache = enabled_cache(100, 60);
    assert!(cache.get("nonexistent-key").await.is_none());
}

#[tokio::test]
async fn cache_hit_returns_value() {
    let cache = enabled_cache(100, 60);
    let result = CompletionResult {
        content: "cached!".to_string(),
        prompt_tokens: 5,
        completion_tokens: 3,
        finish_reason: "stop".to_string(),
    };
    cache.insert("key-1".to_string(), result.clone()).await;
    let hit = cache.get("key-1").await.unwrap();
    assert_eq!(hit.content, "cached!");
    assert_eq!(hit.prompt_tokens, 5);
}

#[tokio::test]
async fn disabled_cache_always_misses() {
    let cache = ResponseCache::new(&CacheConfig {
        enabled: false,
        max_entries: 100,
        ttl_seconds: 60,
    });
    let result = CompletionResult {
        content: "ignored".to_string(),
        prompt_tokens: 1,
        completion_tokens: 1,
        finish_reason: "stop".to_string(),
    };
    cache.insert("key".to_string(), result).await;
    assert!(cache.get("key").await.is_none());
}

#[test]
fn same_inputs_produce_same_key() {
    let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}], "temperature": 0.7, "max_tokens": 100});
    let k1 = make_cache_key(&body);
    let k2 = make_cache_key(&body);
    assert_eq!(k1, k2);
}

#[test]
fn different_model_produces_different_key() {
    let b1 = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]});
    let b2 = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "hello"}]});
    assert_ne!(make_cache_key(&b1), make_cache_key(&b2));
}

#[test]
fn different_messages_produce_different_key() {
    let b1 = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]});
    let b2 = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "world"}]});
    assert_ne!(make_cache_key(&b1), make_cache_key(&b2));
}

#[test]
fn stream_flag_does_not_affect_key() {
    let base = serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]});
    let with_stream_false = serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}], "stream": false});
    let with_stream_true = serde_json::json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}], "stream": true});
    assert_eq!(make_cache_key(&base), make_cache_key(&with_stream_false));
    assert_eq!(make_cache_key(&base), make_cache_key(&with_stream_true));
}

#[test]
fn different_top_p_produces_different_key() {
    let b1 = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}], "top_p": 0.9});
    let b2 = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}], "top_p": 0.5});
    assert_ne!(make_cache_key(&b1), make_cache_key(&b2));
}

// ── Integration tests ──────────────────────────────────────────────────────

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::Settings;
use modelrouter::db::models::NewUser;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    complexity::ComplexityRouter,
    cost::CostCalculator,
    engine::RequestRouter,
    fallback::FallbackChain,
    policy::PolicyEngine,
};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app_with_cache() -> TestServer {
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
        response: "cached response".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries: 10,
        ttl_seconds: 60,
    }));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );
    let state = AppState {
        settings,
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
        app_metrics: None,
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn second_identical_request_returns_cached_response() {
    let server = test_app_with_cache().await;
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hello cache"}]
    });

    // First request — goes to provider
    let resp1 = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&body)
        .await;
    assert_eq!(resp1.status_code(), 200);

    // Second identical request — should hit cache
    let resp2 = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&body)
        .await;
    assert_eq!(resp2.status_code(), 200);

    let b1: serde_json::Value = resp1.json();
    let b2: serde_json::Value = resp2.json();
    assert_eq!(
        b1["choices"][0]["message"]["content"],
        b2["choices"][0]["message"]["content"]
    );
}

#[tokio::test]
async fn streaming_requests_are_not_cached() {
    let server = test_app_with_cache().await;
    let messages = serde_json::json!([{"role": "user", "content": "stream me"}]);

    // Streaming request — should not be cached
    let stream_resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": messages,
            "stream": true
        }))
        .await;
    assert_eq!(stream_resp.status_code(), 200);

    // Non-streaming request with same messages — should succeed (not corrupted by streaming)
    let non_stream_resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": messages
        }))
        .await;
    assert_eq!(non_stream_resp.status_code(), 200);
    let body: serde_json::Value = non_stream_resp.json();
    assert!(body["choices"][0]["message"]["content"].is_string());
}
