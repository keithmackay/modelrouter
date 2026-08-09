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
    let response_cache = Arc::new(ResponseCache::new(&CacheConfig::default()));
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
