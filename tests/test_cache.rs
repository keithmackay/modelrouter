mod common;

use modelrouter::config::schema::CacheConfig;
use modelrouter::providers::adapter::CompletionResult;
use modelrouter::router::cache::store::{CacheStore, CachedEntry, MemoryStore};
use modelrouter::router::cache::{
    completion_cache_key, make_cache_key, search_cache_key, CachePolicy, CachePolicyUpdate,
    ResponseCache,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// A cache with the conservative default policy, but switched on.
fn enabled_cache(max_entries: u64, ttl_seconds: u64) -> ResponseCache {
    ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries,
        ttl_seconds,
        ..Default::default()
    })
}

fn sample_result(content: &str) -> CompletionResult {
    CompletionResult {
        content: content.to_string(),
        prompt_tokens: 5,
        completion_tokens: 3,
        finish_reason: "stop".to_string(),
        ..Default::default()
    }
}

// ── Store round-trip ──────────────────────────────────────────────────────────

#[tokio::test]
async fn cache_miss_returns_none() {
    let cache = enabled_cache(100, 60);
    assert!(cache.get_completion("nonexistent-key", "gpt-4o").await.is_none());
}

#[tokio::test]
async fn cache_hit_returns_value() {
    let cache = enabled_cache(100, 60);
    cache
        .put_completion("key-1", "gpt-4o", &sample_result("cached!"), 0.02)
        .await;
    let hit = cache.get_completion("key-1", "gpt-4o").await.unwrap();
    assert_eq!(hit.content, "cached!");
    assert_eq!(hit.prompt_tokens, 5);
}

#[tokio::test]
async fn stats_track_hits_misses_and_savings() {
    let cache = enabled_cache(100, 60);
    cache
        .put_completion("k", "gpt-4o", &sample_result("hi"), 0.25)
        .await;
    cache.get_completion("k", "gpt-4o").await.unwrap();
    assert!(cache.get_completion("missing", "gpt-4o").await.is_none());

    let stats = cache.stats().await;
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert!((stats.hit_rate - 0.5).abs() < f64::EPSILON);
    assert!((stats.saved_usd - 0.25).abs() < 1e-9);
    assert_eq!(stats.backend, "memory");
    let per_model = stats.by_model.iter().find(|m| m.model == "gpt-4o").unwrap();
    assert_eq!(per_model.hits, 1);
    assert_eq!(per_model.misses, 1);
}

// ── Memory backend ────────────────────────────────────────────────────────────

fn entry(model: &str) -> CachedEntry {
    CachedEntry {
        class: "completion".to_string(),
        model: model.to_string(),
        payload: json!({"content": "x"}),
        original_cost_usd: 0.01,
        stored_at: 0,
        expires_at: 0,
    }
}

#[tokio::test]
async fn memory_store_round_trips_and_counts_entries() {
    let store = MemoryStore::new(&CacheConfig::default());
    store.put("a", entry("m"), Duration::from_secs(60)).await;
    assert!(store.get("a").await.is_some());
    assert_eq!(store.entry_count().await, 1);
    assert_eq!(store.backend_name(), "memory");
    assert!(store.healthy().await);
}

#[tokio::test]
async fn memory_store_honours_ttl() {
    let store = MemoryStore::new(&CacheConfig::default());
    // Sub-second TTLs are clamped to 1s, so this is the shortest observable TTL.
    store.put("a", entry("m"), Duration::from_secs(1)).await;
    assert!(store.get("a").await.is_some());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(store.get("a").await.is_none(), "entry should expire");
}

#[tokio::test]
async fn memory_store_purges_by_key_model_and_all() {
    let store = MemoryStore::new(&CacheConfig::default());
    let fp = modelrouter::router::cache::model_fingerprint("gpt-4o");
    store
        .put(&format!("completion:{}:aaa", fp), entry("gpt-4o"), Duration::from_secs(60))
        .await;
    store
        .put(&format!("completion:{}:bbb", fp), entry("gpt-4o"), Duration::from_secs(60))
        .await;
    let other_fp = modelrouter::router::cache::model_fingerprint("claude");
    store
        .put(&format!("completion:{}:ccc", other_fp), entry("claude"), Duration::from_secs(60))
        .await;

    assert!(store.purge_key(&format!("completion:{}:aaa", fp)).await);
    assert!(!store.purge_key("no-such-key").await);
    assert_eq!(store.purge_model(&fp).await, 1, "only the remaining gpt-4o entry");
    assert!(store.get(&format!("completion:{}:ccc", other_fp)).await.is_some());
    assert_eq!(store.purge_all().await, 1);
    assert_eq!(store.entry_count().await, 0);
}

#[tokio::test]
async fn cache_purge_by_model_leaves_other_models() {
    let cache = enabled_cache(100, 60);
    let gpt_key = completion_cache_key("gpt-4o", &json!({"messages": []}));
    let claude_key = completion_cache_key("claude-opus", &json!({"messages": []}));
    cache.put_completion(&gpt_key, "gpt-4o", &sample_result("a"), 0.0).await;
    cache.put_completion(&claude_key, "claude-opus", &sample_result("b"), 0.0).await;

    assert_eq!(cache.purge_model("gpt-4o").await, 1);
    assert!(cache.get_completion(&gpt_key, "gpt-4o").await.is_none());
    assert!(cache.get_completion(&claude_key, "claude-opus").await.is_some());
}

#[tokio::test]
async fn disabled_cache_is_never_eligible() {
    let cache = ResponseCache::new(&CacheConfig::default());
    assert!(!cache.completion_eligible(&json!({"temperature": 0.0})));
    assert!(!cache.search_eligible());
}

#[tokio::test]
async fn backend_selection_is_config_driven_and_fails_safe() {
    use modelrouter::router::cache::store::{build_store, RedisStore};

    // Explicit memory backend.
    let memory = build_store(&CacheConfig::default());
    assert_eq!(memory.backend_name(), "memory");

    // Redis requested with no URL: fall back to memory rather than fail requests.
    let no_url = build_store(&CacheConfig {
        backend: "redis".to_string(),
        ..Default::default()
    });
    assert_eq!(no_url.backend_name(), "memory");

    // An unrecognised backend name also falls back.
    let unknown = build_store(&CacheConfig {
        backend: "hypercache".to_string(),
        ..Default::default()
    });
    assert_eq!(unknown.backend_name(), "memory");

    // A well-formed URL yields a Redis store even with nothing listening; the
    // store reports unhealthy and misses rather than erroring.
    let redis = RedisStore::new("redis://127.0.0.1:63999", "test-ns").unwrap();
    assert_eq!(redis.backend_name(), "redis");
    assert!(!redis.healthy().await);
    assert!(redis.get("anything").await.is_none());

    // A malformed URL is a construction error, not a panic.
    assert!(RedisStore::new("not-a-url", "test-ns").is_err());
}

// ── Key derivation ────────────────────────────────────────────────────────────

#[test]
fn same_inputs_produce_same_key() {
    let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}], "temperature": 0.7, "max_tokens": 100});
    assert_eq!(make_cache_key(&body), make_cache_key(&body));
}

#[test]
fn field_order_does_not_affect_key() {
    let a = json!({"model": "gpt-4o", "temperature": 0.0});
    let b = json!({"temperature": 0.0, "model": "gpt-4o"});
    assert_eq!(make_cache_key(&a), make_cache_key(&b));
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
fn transport_only_fields_do_not_affect_key() {
    let base = json!({"model": "gpt-4o", "messages": []});
    for extra in [
        json!({"model": "gpt-4o", "messages": [], "stream": true}),
        json!({"model": "gpt-4o", "messages": [], "stream": false}),
        json!({"model": "gpt-4o", "messages": [], "user": "alice"}),
        json!({"model": "gpt-4o", "messages": [], "session_id": "s-1"}),
        json!({"model": "gpt-4o", "messages": [], "_mr_session_window_secs": 900}),
    ] {
        assert_eq!(make_cache_key(&base), make_cache_key(&extra), "{}", extra);
    }
}

#[test]
fn every_sampling_parameter_changes_the_key() {
    let base = json!({"model": "gpt-4o", "messages": [], "temperature": 0.0});
    for changed in [
        json!({"model": "gpt-4o", "messages": [], "temperature": 0.5}),
        json!({"model": "gpt-4o", "messages": [], "temperature": 0.0, "top_p": 0.5}),
        json!({"model": "gpt-4o", "messages": [], "temperature": 0.0, "max_tokens": 100}),
        json!({"model": "gpt-4o", "messages": [], "temperature": 0.0, "seed": 7}),
        json!({"model": "gpt-4o", "messages": [], "temperature": 0.0, "response_format": {"type": "json_object"}}),
        json!({"model": "gpt-4o", "messages": [], "temperature": 0.0, "tools": [{"type": "function"}]}),
    ] {
        assert_ne!(make_cache_key(&base), make_cache_key(&changed), "{}", changed);
    }
}

#[test]
fn resolved_model_is_part_of_the_completion_key() {
    let body = json!({"model": "fast", "messages": []});
    assert_ne!(
        completion_cache_key("gpt-4o-mini", &body),
        completion_cache_key("claude-haiku", &body),
        "an alias re-pointed at another model must not reuse the entry"
    );
}

#[test]
fn search_key_covers_engine_query_and_options() {
    let base = search_cache_key("tavily", "rust", Some(5));
    assert_eq!(base, search_cache_key("tavily", "rust", Some(5)));
    assert_ne!(base, search_cache_key("tavily", "rust", Some(10)));
    assert_ne!(base, search_cache_key("tavily", "go", Some(5)));
    assert_ne!(base, search_cache_key("brave", "rust", Some(5)));
}

// ── Eligibility policy ────────────────────────────────────────────────────────

#[test]
fn zero_temperature_is_eligible_by_default() {
    let cache = enabled_cache(10, 60);
    assert!(cache.completion_eligible(&json!({"temperature": 0.0, "messages": []})));
}

#[test]
fn high_temperature_is_not_cached() {
    let cache = enabled_cache(10, 60);
    assert!(!cache.completion_eligible(&json!({"temperature": 0.7, "messages": []})));
    assert!(!cache.completion_eligible(&json!({"temperature": 1.0, "messages": []})));
}

#[test]
fn omitted_temperature_is_not_cached_by_default() {
    let cache = enabled_cache(10, 60);
    assert!(
        !cache.completion_eligible(&json!({"messages": []})),
        "an omitted temperature means the provider default (1.0), not 0.0"
    );
}

#[test]
fn streaming_is_never_cached() {
    let cache = enabled_cache(10, 60);
    assert!(!cache.completion_eligible(&json!({"temperature": 0.0, "stream": true})));
}

#[test]
fn raising_the_threshold_makes_warmer_requests_eligible() {
    let cache = enabled_cache(10, 60);
    cache.update_policy(&CachePolicyUpdate {
        completions_max_temperature: Some(0.5),
        ..Default::default()
    });
    assert!(cache.completion_eligible(&json!({"temperature": 0.5, "messages": []})));
    assert!(!cache.completion_eligible(&json!({"temperature": 0.6, "messages": []})));
}

#[test]
fn policy_update_only_changes_supplied_fields() {
    let cache = enabled_cache(10, 60);
    let before = cache.policy();
    cache.update_policy(&CachePolicyUpdate {
        search_ttl_seconds: Some(42),
        ..Default::default()
    });
    let after = cache.policy();
    assert_eq!(after.search.ttl_seconds, 42);
    assert_eq!(after.completions.max_temperature, before.completions.max_temperature);
    assert_eq!(after.enabled, before.enabled);
}

#[test]
fn disabling_a_class_disables_only_that_class() {
    let cache = enabled_cache(10, 60);
    cache.update_policy(&CachePolicyUpdate {
        completions_enabled: Some(false),
        ..Default::default()
    });
    assert!(!cache.completion_eligible(&json!({"temperature": 0.0})));
    assert!(cache.search_eligible());
}

#[test]
fn policy_from_config_carries_conservative_defaults() {
    let policy = CachePolicy::from_config(&CacheConfig::default());
    assert!(!policy.enabled, "the cache is off unless explicitly enabled");
    assert_eq!(policy.completions.max_temperature, 0.0);
    assert_eq!(policy.completions.assumed_temperature, 1.0);
    assert_eq!(policy.completion_ttl(), Duration::from_secs(3600));
    assert_eq!(policy.search_ttl(), Duration::from_secs(900));
}

// ── Integration ───────────────────────────────────────────────────────────────

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::Settings;
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use std::collections::HashMap;

async fn test_app_with_cache() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    db.create(NewUser {
        name: "test-user".to_string(),
        email: None,
    })
    .await
    .unwrap();

    let user = UserRepository::find_by_name(&db, "test-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(
        &db,
        NewApiKey {
            user_id: user.id,
            key_hash: hash_token("test-token"),
            label: Some("test".to_string()),
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let response_cache = Arc::new(ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries: 10,
        ttl_seconds: 60,
        ..Default::default()
    }));

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "cached response".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache,
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
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

async fn post_completion(server: &TestServer, body: &serde_json::Value) -> axum_test::TestResponse {
    server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(body)
        .await
}

#[tokio::test]
async fn second_identical_request_is_served_from_cache() {
    let (server, _db) = test_app_with_cache().await;
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hello cache"}],
        "temperature": 0.0
    });

    let first = post_completion(&server, &body).await;
    assert_eq!(first.status_code(), 200);
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");

    let second = post_completion(&server, &body).await;
    assert_eq!(second.status_code(), 200);
    assert_eq!(second.headers().get("x-modelrouter-cache").unwrap(), "HIT");

    let b1: serde_json::Value = first.json();
    let b2: serde_json::Value = second.json();
    assert_eq!(
        b1["choices"][0]["message"]["content"],
        b2["choices"][0]["message"]["content"]
    );
}

#[tokio::test]
async fn changed_sampling_parameter_misses() {
    let (server, _db) = test_app_with_cache().await;
    let base = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "same question"}],
        "temperature": 0.0
    });
    assert_eq!(post_completion(&server, &base).await.status_code(), 200);

    let with_max_tokens = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "same question"}],
        "temperature": 0.0,
        "max_tokens": 64
    });
    let resp = post_completion(&server, &with_max_tokens).await;
    assert_eq!(
        resp.headers().get("x-modelrouter-cache").unwrap(),
        "MISS",
        "a different max_tokens is a different request"
    );
}

#[tokio::test]
async fn high_temperature_requests_never_hit() {
    let (server, _db) = test_app_with_cache().await;
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "be creative"}],
        "temperature": 0.9
    });
    for _ in 0..2 {
        let resp = post_completion(&server, &body).await;
        assert_eq!(resp.status_code(), 200);
        assert_eq!(resp.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    }
}

#[tokio::test]
async fn cache_hit_is_metered_with_zero_cost() {
    use modelrouter::db::repositories::costs::CostRepository;

    let (server, db) = test_app_with_cache().await;
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "meter me"}],
        "temperature": 0.0
    });
    assert_eq!(post_completion(&server, &body).await.status_code(), 200);
    assert_eq!(post_completion(&server, &body).await.status_code(), 200);

    // Cost recording is fire-and-forget; give the spawned tasks a moment.
    let since = "1970-01-01T00:00:00Z";
    let mut summary = CostRepository::cache_summary_since(&*db, None, since).await.unwrap();
    for _ in 0..40 {
        if summary.hits >= 1 && summary.requests >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        summary = CostRepository::cache_summary_since(&*db, None, since).await.unwrap();
    }

    assert_eq!(summary.hits, 1, "the second request is a metered cache hit");
    assert_eq!(summary.requests, 2, "both requests are usage records");
    assert!((summary.hit_rate() - 0.5).abs() < 1e-9);

    let entries = CostRepository::list_cost_entries_before(&*db, "2999-01-01T00:00:00Z")
        .await
        .unwrap();
    let hit = entries.iter().find(|e| e.cache_hit).expect("a cache_hit row");
    assert_eq!(hit.cost_usd, 0.0, "a cache hit must never be counted as spend");
    assert!(hit.saved_usd >= 0.0);
}

#[tokio::test]
async fn streaming_requests_are_not_cached() {
    let (server, _db) = test_app_with_cache().await;
    let messages = json!([{"role": "user", "content": "stream me"}]);

    let stream_resp = post_completion(
        &server,
        &json!({"model": "gpt-4o", "messages": messages, "temperature": 0.0, "stream": true}),
    )
    .await;
    assert_eq!(stream_resp.status_code(), 200);

    let non_stream = post_completion(
        &server,
        &json!({"model": "gpt-4o", "messages": messages, "temperature": 0.0}),
    )
    .await;
    assert_eq!(non_stream.status_code(), 200);
    assert_eq!(
        non_stream.headers().get("x-modelrouter-cache").unwrap(),
        "MISS",
        "a streamed response must not populate the cache"
    );
    let body: serde_json::Value = non_stream.json();
    assert!(body["choices"][0]["message"]["content"].is_string());
}
