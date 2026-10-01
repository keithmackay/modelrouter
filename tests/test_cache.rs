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
fn streaming_is_eligible_and_shares_the_plain_key() {
    let cache = enabled_cache(10, 60);
    let streamed = json!({"temperature": 0.0, "stream": true, "messages": []});
    let plain = json!({"temperature": 0.0, "messages": []});
    assert!(cache.completion_eligible(&streamed));
    assert_eq!(
        completion_cache_key("gpt-4o", &streamed),
        completion_cache_key("gpt-4o", &plain)
    );
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
    let (server, db, _cache) = test_app_with_adapter(common::MockAdapter {
        response: "cached response".to_string(),
    })
    .await;
    (server, db)
}

async fn test_app_with_adapter<A: modelrouter::providers::adapter::ProviderAdapter + 'static>(
    adapter: A,
) -> (TestServer, Arc<dyn DatabaseProvider>, Arc<ResponseCache>) {
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
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(adapter)),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: response_cache.clone(),
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
    (
        TestServer::new(build_router(state)).unwrap(),
        db,
        response_cache,
    )
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
    let first = post_completion(&server, &body).await;
    assert_eq!(first.status_code(), 200);
    let second = post_completion(&server, &body).await;
    assert_eq!(second.status_code(), 200);
    let miss: serde_json::Value = first.json();
    let hit_body: serde_json::Value = second.json();

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

    // The responses report exactly what the ledger recorded for each call.
    // Tolerance only for serde_json's non-round-trip float parsing in the test.
    let close = |v: &serde_json::Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-15;
    let live = entries.iter().find(|e| !e.cache_hit).expect("a live row");
    assert!(close(&miss["usage"]["cost_usd"], live.cost_usd));
    assert!(miss["usage"].get("cache_hit").is_none());
    assert_eq!(miss["x_router"]["cost"]["cache_hit"], false);
    assert_eq!(hit_body["usage"]["cost_usd"].as_f64(), Some(0.0));
    assert_eq!(hit_body["usage"]["cache_hit"], true);
    assert!(close(&hit_body["usage"]["saved_usd"], hit.saved_usd));
    let meta = &hit_body["x_router"];
    assert_eq!(meta["cost"]["cost_usd"].as_f64(), Some(0.0));
    assert_eq!(meta["cost"]["cache_hit"], true);
    assert!(close(&meta["cost"]["saved_usd"], hit.saved_usd));
    assert_eq!(meta["model"], hit.model.as_str(), "a hit names the model that produced it");
    assert_eq!(meta["timing"]["attempts"], 0, "no provider call on a hit");
    assert!(meta["timing"]["provider_ms"].is_null());
}

/// Streams like a real adapter: usage in a final `choices: []` chunk, and
/// chunk boundaries that fall mid-line. Counts provider calls.
#[derive(Clone, Default)]
struct StreamingAdapter {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    /// Break the stream with an error after the first chunk.
    fail_mid_stream: bool,
}

impl StreamingAdapter {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl modelrouter::providers::adapter::ProviderAdapter for StreamingAdapter {
    async fn complete(
        &self,
        _req: &modelrouter::providers::adapter::NormalizedRequest,
    ) -> anyhow::Result<CompletionResult> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(CompletionResult {
            content: "Hello world".to_string(),
            prompt_tokens: 40,
            completion_tokens: 2,
            finish_reason: "stop".to_string(),
            ..Default::default()
        })
    }

    async fn stream(
        &self,
        _req: &modelrouter::providers::adapter::NormalizedRequest,
    ) -> anyhow::Result<modelrouter::providers::adapter::SseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let sse = [
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello"}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": " world"}, "finish_reason": "stop"}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "choices": [], "usage": {"prompt_tokens": 40, "completion_tokens": 2, "total_tokens": 42}}),
        ]
        .iter()
        .map(|c| format!("data: {c}\n\n"))
        .collect::<String>()
            + "data: [DONE]\n\n";
        let (head, tail) = sse.split_at(sse.len() / 2);
        let mut chunks: Vec<anyhow::Result<bytes::Bytes>> =
            vec![Ok(bytes::Bytes::from(head.to_string()))];
        if self.fail_mid_stream {
            chunks.push(Err(anyhow::anyhow!("upstream reset")));
        }
        chunks.push(Ok(bytes::Bytes::from(tail.to_string())));
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

async fn wait_for_stores(cache: &ResponseCache, want: u64) {
    for _ in 0..200 {
        if cache.stats().await.stores >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {want} cache stores");
}

/// The `data:` payloads of an SSE body; `[DONE]` as a JSON string.
fn sse_data(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap_or_else(|_| json!(d)))
        .collect()
}

fn ask(stream: bool) -> serde_json::Value {
    json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "say hello"}],
        "temperature": 0.0,
        "stream": stream
    })
}

#[tokio::test]
async fn completed_stream_is_stored_and_replayed_as_sse() {
    let adapter = StreamingAdapter::default();
    let (server, _db, cache) = test_app_with_adapter(adapter.clone()).await;

    let live = post_completion(&server, &ask(true)).await;
    assert_eq!(live.status_code(), 200);
    assert_eq!(live.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    wait_for_stores(&cache, 1).await;

    let replay = post_completion(&server, &ask(true)).await;
    assert_eq!(replay.status_code(), 200);
    assert_eq!(replay.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    assert_eq!(
        replay.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(adapter.calls(), 1, "the replay never reaches the provider");

    let events = sse_data(&replay.text());
    assert_eq!(events.last().unwrap(), "[DONE]");
    let text: String = events
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello world");
    assert!(events
        .iter()
        .any(|e| e["choices"][0]["finish_reason"] == "stop"));
    let usage = &events[events.len() - 2]["usage"];
    assert_eq!(
        (
            usage["prompt_tokens"].as_u64(),
            usage["completion_tokens"].as_u64()
        ),
        (Some(40), Some(2))
    );
    assert_eq!(usage["cost_usd"].as_f64(), Some(0.0));
    assert_eq!(usage["cache_hit"], true);
}

#[tokio::test]
async fn streamed_entry_serves_a_plain_request_and_the_reverse() {
    let adapter = StreamingAdapter::default();
    let (server, _db, cache) = test_app_with_adapter(adapter.clone()).await;

    assert_eq!(
        post_completion(&server, &ask(true)).await.status_code(),
        200
    );
    wait_for_stores(&cache, 1).await;
    let plain = post_completion(&server, &ask(false)).await;
    assert_eq!(plain.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    let body: serde_json::Value = plain.json();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello world");
    assert_eq!(body["usage"]["prompt_tokens"], 40);

    let other = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "other"}], "temperature": 0.0});
    assert_eq!(
        post_completion(&server, &other)
            .await
            .headers()
            .get("x-modelrouter-cache")
            .unwrap(),
        "MISS"
    );
    let mut other_stream = other.clone();
    other_stream["stream"] = json!(true);
    let replay = post_completion(&server, &other_stream).await;
    assert_eq!(replay.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    let text: String = sse_data(&replay.text())
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello world");
    assert_eq!(adapter.calls(), 2);
}

#[tokio::test]
async fn errored_stream_is_not_stored() {
    let adapter = StreamingAdapter {
        fail_mid_stream: true,
        ..Default::default()
    };
    let (server, _db, cache) = test_app_with_adapter(adapter.clone()).await;
    // axum-test panics on a body that errors mid-stream, which is the point.
    use futures::FutureExt;
    let broken = std::panic::AssertUnwindSafe(post_completion(&server, &ask(true)))
        .catch_unwind()
        .await;
    assert!(broken.is_err(), "the client sees the stream break");
    let _ = post_completion(&server, &ask(false)).await;
    assert_eq!(adapter.calls(), 2, "the broken stream left nothing to hit");
    assert_eq!(cache.stats().await.stores, 1, "only the plain call stored");
}

#[tokio::test]
async fn stream_without_usage_is_not_stored() {
    // `common::MockAdapter` streams no usage chunk; a hit replaying it would
    // have no real token counts to report.
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
        "MISS"
    );
}

#[tokio::test]
async fn stream_replay_is_metered_as_a_cache_hit() {
    let adapter = StreamingAdapter::default();
    let (server, db, cache) = test_app_with_adapter(adapter).await;
    post_completion(&server, &ask(true)).await;
    wait_for_stores(&cache, 1).await;
    post_completion(&server, &ask(true)).await;

    let rows = common::wait_for_ledger_rows(&*db, 2).await;
    let hit = rows.iter().find(|r| r.cache_hit).expect("a cache-hit row");
    assert_eq!(hit.cost_usd, 0.0);
    assert!(hit.saved_usd > 0.0);
    assert_eq!((hit.tokens_in, hit.tokens_out), (40, 2));
    assert_eq!(cache.stats().await.hits, 1);
}
