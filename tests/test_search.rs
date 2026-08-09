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
    let search_registry = Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
        results: mock_results(),
    }));

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
