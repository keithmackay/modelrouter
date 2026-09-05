mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, PricingEntry, Settings};
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search::SearchResultItem,
    search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use std::collections::HashMap;
use std::sync::Arc;

fn mock_results() -> Vec<SearchResultItem> {
    vec![SearchResultItem {
        title: "Example Domain".to_string(),
        url: "https://example.com".to_string(),
        snippet: "Example description".to_string(),
        score: Some(0.9),
        published_date: None,
    }]
}

async fn test_app_with_pricing(
    pricing: Vec<PricingEntry>,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    test_app_with_pricing_and_cache(pricing, CacheConfig::default()).await
}

async fn test_app_with_pricing_and_cache(
    pricing: Vec<PricingEntry>,
    cache: CacheConfig,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    test_app_full(
        pricing,
        cache,
        SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: mock_results(),
        }),
        None,
    )
    .await
}

/// Full harness: lets a test choose which search engines the registry can serve
/// and what `[routing] default_search_engine` says, which is what the
/// engine-resolution tests below vary.
async fn test_app_full(
    pricing: Vec<PricingEntry>,
    cache: CacheConfig,
    search_registry: SearchRegistry,
    default_search_engine: Option<&str>,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    UserRepository::create(
        &db,
        NewUser {
            name: "test-user".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let user = UserRepository::find_by_name(&db, "test-user")
        .await
        .unwrap()
        .unwrap();
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

    let mut settings = Settings::default();
    settings.pricing = pricing;
    settings.routing.default_search_engine = default_search_engine.map(str::to_string);
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "hello".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(ResponseCache::new(&cache));
    let embedding_registry = Arc::new(EmbeddingRegistry::new_with_mock(
        common::MockEmbeddingAdapter {
            embedding: vec![0.1_f32, 0.2, 0.3],
        },
    ));
    let search_registry = Arc::new(search_registry);

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
        search_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(
            0, 0,
        )),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
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

async fn test_app() -> TestServer {
    test_app_with_pricing(vec![]).await.0
}

#[tokio::test]
async fn search_unauthenticated_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/search")
        .json(&serde_json::json!({ "query": "rust programming language" }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn search_happy_path_returns_normalized_results_and_usage() {
    let server = test_app().await;
    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust programming language" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["engine"], "tavily");
    let results = body["results"]
        .as_array()
        .expect("results should be an array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Example Domain");
    assert_eq!(results[0]["url"], "https://example.com");
    assert_eq!(results[0]["snippet"], "Example description");
    assert_eq!(results[0]["score"], 0.9);
    assert!(results[0]["published_date"].is_null());
    assert_eq!(body["usage"]["results"], 1);
    assert!(body["usage"]["cost_usd"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn search_empty_query_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "" }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn search_unknown_engine_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "engine": "bing" }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

// ── Engine resolution when the request omits `engine` ────────────────────────
//
// These cover the regression that made a Vertex-only host answer every
// engine-less `/v1/search` with `502 No search adapter configured for engine:
// tavily`: the route used to hardcode `unwrap_or("tavily")` regardless of what
// the operator had configured.

fn mock_search_registry(engines: &[&str]) -> SearchRegistry {
    SearchRegistry::new_with_mock_engines(
        engines
            .iter()
            .map(|e| {
                let adapter: Arc<dyn modelrouter::providers::search::SearchAdapter> =
                    Arc::new(common::MockSearchAdapter {
                        results: mock_results(),
                    });
                (*e, adapter)
            })
            .collect(),
    )
}

async fn search_without_engine(
    registry: SearchRegistry,
    default_search_engine: Option<&str>,
) -> axum_test::TestResponse {
    let (server, _db) = test_app_full(
        vec![],
        CacheConfig::default(),
        registry,
        default_search_engine,
    )
    .await;
    server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust" }))
        .await
}

/// The sole available engine serves the request even when it is not Tavily.
#[cfg(feature = "vertex")]
#[tokio::test]
async fn search_omitted_engine_resolves_to_sole_configured_engine() {
    let resp = search_without_engine(mock_search_registry(&["vertex"]), None).await;
    assert_eq!(resp.status_code(), 200);
}

/// With more than one engine available and no configured default, refuse and
/// name the options rather than silently substituting one of them.
#[cfg(feature = "vertex")]
#[tokio::test]
async fn search_omitted_engine_with_multiple_engines_returns_400() {
    let resp = search_without_engine(mock_search_registry(&["tavily", "vertex"]), None).await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("tavily"), "message was: {message}");
    assert!(message.contains("vertex"), "message was: {message}");
}

/// `[routing] default_search_engine` resolves the ambiguity above.
#[cfg(feature = "vertex")]
#[tokio::test]
async fn search_omitted_engine_uses_configured_default() {
    let resp =
        search_without_engine(mock_search_registry(&["tavily", "vertex"]), Some("vertex")).await;
    assert_eq!(resp.status_code(), 200);
}

/// An explicit `engine` still wins over the configured default.
#[cfg(feature = "vertex")]
#[tokio::test]
async fn search_explicit_engine_overrides_configured_default() {
    let (server, _db) = test_app_full(
        vec![],
        CacheConfig::default(),
        mock_search_registry(&["tavily", "vertex"]),
        Some("vertex"),
    )
    .await;
    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "engine": "bing" }))
        .await;
    // `bing` is unsupported: proof the request's own engine was used, not the
    // default that would have answered 200.
    assert_eq!(resp.status_code(), 400);
}

/// No engines at all is a configuration error the operator can act on, not a
/// 502 naming an engine they never configured.
#[tokio::test]
async fn search_omitted_engine_with_no_engines_returns_400() {
    let resp = search_without_engine(SearchRegistry::new(HashMap::new()), None).await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no search engine configured"),
        "message was: {message}"
    );
}

#[tokio::test]
async fn search_max_results_out_of_bounds_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "max_results": 0 }))
        .await;
    assert_eq!(resp.status_code(), 400);

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "max_results": 1000 }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn search_writes_cost_ledger_row_with_configured_pricing() {
    let (server, db) = test_app_with_pricing(vec![PricingEntry {
        model: "search/tavily".to_string(),
        input_per_million: 0.25,
        output_per_million: 0.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }])
    .await;

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust programming language" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["usage"]["cost_usd"], 0.25);

    // Cost recording happens on a spawned task; poll briefly for it to land.
    let user = UserRepository::find_by_name(&*db, "test-user")
        .await
        .unwrap()
        .unwrap();
    let mut total = 0.0;
    for _ in 0..20 {
        total = CostRepository::sum_for_user_since(&*db, user.id, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
        if total > 0.0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        (total - 0.25).abs() < 0.000001,
        "expected cost ledger sum 0.25, got {total}"
    );
}

// ── Response cache ────────────────────────────────────────────────────────────

fn enabled_cache_config() -> CacheConfig {
    CacheConfig {
        enabled: true,
        max_entries: 10,
        ttl_seconds: 60,
        ..Default::default()
    }
}

fn search_pricing() -> Vec<PricingEntry> {
    vec![PricingEntry {
        model: "search/tavily".to_string(),
        input_per_million: 0.25,
        output_per_million: 0.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }]
}

async fn post_search(server: &TestServer, body: serde_json::Value) -> axum_test::TestResponse {
    server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&body)
        .await
}

#[tokio::test]
async fn repeated_search_is_served_from_cache_at_zero_cost() {
    let (server, db) =
        test_app_with_pricing_and_cache(search_pricing(), enabled_cache_config()).await;
    let query = serde_json::json!({ "query": "rust programming language", "max_results": 5 });

    let first = post_search(&server, query.clone()).await;
    assert_eq!(first.status_code(), 200);
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");
    assert_eq!(first.json::<serde_json::Value>()["usage"]["cost_usd"], 0.25);

    let second = post_search(&server, query).await;
    assert_eq!(second.status_code(), 200);
    assert_eq!(second.headers().get("x-modelrouter-cache").unwrap(), "HIT");
    let body: serde_json::Value = second.json();
    assert_eq!(body["usage"]["cost_usd"], 0.0);
    assert_eq!(body["usage"]["cache_hit"], true);
    assert_eq!(body["usage"]["saved_usd"], 0.25);
    assert_eq!(body["results"][0]["url"], "https://example.com");

    // The hit is metered: two usage rows, one of them a cache hit, and total
    // spend is unchanged from the single live call.
    let mut summary = CostRepository::cache_summary_since(&*db, None, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    for _ in 0..40 {
        if summary.requests >= 2 && summary.hits >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        summary = CostRepository::cache_summary_since(&*db, None, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
    }
    assert_eq!(summary.hits, 1);
    assert_eq!(summary.requests, 2);
    assert!((summary.saved_usd - 0.25).abs() < 1e-9);

    let spend = CostRepository::sum_global_since(&*db, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    assert!(
        (spend - 0.25).abs() < 1e-9,
        "a cache hit must not add spend, got {spend}"
    );
}

#[tokio::test]
async fn different_search_options_are_different_cache_entries() {
    let (server, _db) =
        test_app_with_pricing_and_cache(search_pricing(), enabled_cache_config()).await;
    let first = post_search(
        &server,
        serde_json::json!({ "query": "rust", "max_results": 5 }),
    )
    .await;
    assert_eq!(first.headers().get("x-modelrouter-cache").unwrap(), "MISS");

    let second = post_search(
        &server,
        serde_json::json!({ "query": "rust", "max_results": 3 }),
    )
    .await;
    assert_eq!(
        second.headers().get("x-modelrouter-cache").unwrap(),
        "MISS",
        "max_results is part of the search cache key"
    );
}

#[tokio::test]
async fn search_is_not_cached_when_the_cache_is_disabled() {
    let (server, _db) = test_app_with_pricing(search_pricing()).await;
    let query = serde_json::json!({ "query": "rust" });
    assert_eq!(
        post_search(&server, query.clone())
            .await
            .headers()
            .get("x-modelrouter-cache")
            .unwrap(),
        "MISS"
    );
    assert_eq!(
        post_search(&server, query)
            .await
            .headers()
            .get("x-modelrouter-cache")
            .unwrap(),
        "MISS"
    );
}

// ── Search engine fallback chains (issue #42) ────────────────────────────────

/// Build a test app with a custom search registry and fallback chain config.
async fn test_app_with_chain(
    registry: SearchRegistry,
    chains: HashMap<String, Vec<String>>,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    UserRepository::create(
        &db,
        NewUser {
            name: "test-user".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let user = UserRepository::find_by_name(&db, "test-user")
        .await
        .unwrap()
        .unwrap();
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

    let mut settings = Settings::default();
    settings.routing.search_fallback_chains = chains;
    let settings = Arc::new(settings);
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
    let search_registry = Arc::new(registry);

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
        search_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(
            0, 0,
        )),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
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

#[tokio::test]
async fn primary_engine_fails_fallback_chain_is_walked() {
    // Primary "tavily" errors, fallback to "vertex" succeeds.
    let registry = SearchRegistry::new_with_mock_engines(vec![
        (
            "tavily",
            Arc::new(common::FailingSearchAdapter {
                error_message: "simulated tavily timeout".to_string(),
            }),
        ),
        (
            "vertex",
            Arc::new(common::NamedMockSearchAdapter {
                results: mock_results(),
                engine_name: "vertex".to_string(),
            }),
        ),
    ]);

    let mut chains = HashMap::new();
    chains.insert("tavily".to_string(), vec!["vertex".to_string()]);

    let (server, _db) = test_app_with_chain(registry, chains).await;

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "engine": "tavily" }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    // The response metadata must name the serving engine, not the requested one.
    assert_eq!(body["engine"], "vertex");
    assert_eq!(body["results"][0]["url"], "https://example.com");
}

#[tokio::test]
async fn fallback_chain_exhaustion_surfaces_last_error() {
    // Both engines fail; the last error is returned.
    let registry = SearchRegistry::new_with_mock_engines(vec![
        (
            "tavily",
            Arc::new(common::FailingSearchAdapter {
                error_message: "tavily 503 Service Unavailable".to_string(),
            }),
        ),
        (
            "vertex",
            Arc::new(common::FailingSearchAdapter {
                error_message: "vertex rate limit exceeded".to_string(),
            }),
        ),
    ]);

    let mut chains = HashMap::new();
    chains.insert("tavily".to_string(), vec!["vertex".to_string()]);

    let (server, _db) = test_app_with_chain(registry, chains).await;

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "engine": "tavily" }))
        .await;

    assert_eq!(resp.status_code(), 502);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap();
    // The last error in the chain (vertex) should be returned.
    assert!(
        message.contains("vertex rate limit exceeded"),
        "expected last error in message, got: {message}"
    );
}

#[tokio::test]
async fn no_chain_configured_behavior_unchanged() {
    // Without a fallback chain, a single engine failure surfaces immediately.
    let registry = SearchRegistry::new_with_mock_engines(vec![(
        "tavily",
        Arc::new(common::FailingSearchAdapter {
            error_message: "tavily down".to_string(),
        }),
    )]);

    let (server, _db) = test_app_with_chain(registry, HashMap::new()).await;

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "engine": "tavily" }))
        .await;

    assert_eq!(resp.status_code(), 502);
    let body: serde_json::Value = resp.json();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("tavily down"));
}

#[tokio::test]
async fn unconfigured_chain_entry_is_skipped() {
    // Chain lists "bing" (unconfigured), then "vertex" (configured). "bing" is
    // skipped; "vertex" serves.
    let registry = SearchRegistry::new_with_mock_engines(vec![
        (
            "tavily",
            Arc::new(common::FailingSearchAdapter {
                error_message: "tavily down".to_string(),
            }),
        ),
        (
            "vertex",
            Arc::new(common::NamedMockSearchAdapter {
                results: mock_results(),
                engine_name: "vertex".to_string(),
            }),
        ),
    ]);

    let mut chains = HashMap::new();
    // "bing" is not registered in the registry above, so it should be skipped.
    chains.insert("tavily".to_string(), vec!["bing".to_string(), "vertex".to_string()]);

    let (server, _db) = test_app_with_chain(registry, chains).await;

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "rust", "engine": "tavily" }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["engine"], "vertex");
}

#[tokio::test]
async fn caller_error_does_not_trigger_failover() {
    // Invalid query (empty) is a caller error (400), not a provider error.
    // Fallback chains are only walked on provider errors, so this should fail
    // immediately with 400, not try the next engine.
    let registry = SearchRegistry::new_with_mock_engines(vec![
        (
            "tavily",
            Arc::new(common::MockSearchAdapter {
                results: mock_results(),
            }),
        ),
        (
            "vertex",
            Arc::new(common::NamedMockSearchAdapter {
                results: mock_results(),
                engine_name: "vertex".to_string(),
            }),
        ),
    ]);

    let mut chains = HashMap::new();
    chains.insert("tavily".to_string(), vec!["vertex".to_string()]);

    let (server, _db) = test_app_with_chain(registry, chains).await;

    let resp = server
        .post("/v1/search")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({ "query": "", "engine": "tavily" }))
        .await;

    // Caller error (empty query) returns 400 immediately without attempting any
    // engine, so fallback does not happen.
    assert_eq!(resp.status_code(), 400);
}
