mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, Settings};
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

/// Build a full app. `with_mocks` controls whether the provider registries
/// carry a working adapter (deep-health capabilities probe "ok") or are empty
/// (nothing configured — capabilities must read "skipped").
async fn test_app(cache: CacheConfig, with_mocks: bool) -> TestServer {
    let db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

    let provider_registry = if with_mocks {
        Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "pong".to_string(),
        }))
    } else {
        Arc::new(ProviderRegistry::new(HashMap::new()))
    };
    let embedding_registry = if with_mocks {
        Arc::new(EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1, 0.2] },
        ))
    } else {
        Arc::new(EmbeddingRegistry::new(HashMap::new()))
    };
    let search_registry = if with_mocks {
        Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: vec![SearchResultItem {
                title: "t".to_string(),
                url: "https://example.com".to_string(),
                snippet: "s".to_string(),
                score: None,
                published_date: None,
            }],
        }))
    } else {
        Arc::new(SearchRegistry::new(HashMap::new()))
    };

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry,
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&cache)),
        embedding_registry,
        search_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    TestServer::new(build_router(state)).unwrap()
}

// ── GET /health ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_without_caching_is_plain_ok() {
    let server = test_app(CacheConfig::default(), true).await;
    let resp = server.get("/health").await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["status"], "ok");
    assert!(
        body.get("cache").is_none(),
        "cache block must be absent when caching is disabled"
    );
}

#[tokio::test]
async fn health_reports_cache_block_when_caching_enabled() {
    let server = test_app(
        CacheConfig {
            enabled: true,
            namespace: "test-ns".to_string(),
            ..Default::default()
        },
        true,
    )
    .await;
    let resp = server.get("/health").await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    // Cache state never degrades liveness: requests are served live.
    assert_eq!(body["status"], "ok");
    assert_eq!(body["cache"]["backend"], "memory");
    assert_eq!(body["cache"]["connected"], true);
    assert_eq!(body["cache"]["namespace"], "test-ns");
    assert_eq!(body["cache"]["entries"], 0);
}

// ── GET /health/deep ──────────────────────────────────────────────────────────

#[tokio::test]
async fn deep_health_with_no_providers_is_all_skipped_but_well_formed() {
    let server = test_app(CacheConfig::default(), false).await;
    let resp = server.get("/health/deep").await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();

    // Nothing configured -> nothing failed -> the gateway itself is "ok".
    assert_eq!(body["status"], "ok");
    assert_eq!(body["cached"], false);
    assert!(body["checked_at"].as_i64().unwrap() > 0);
    for cap in ["llm", "embedding", "search"] {
        let entry = &body["capabilities"][cap];
        assert_eq!(entry["status"], "skipped", "capability {}", cap);
        assert!(entry["target"].is_string());
        assert!(entry["latency_ms"].is_i64() || entry["latency_ms"].is_u64());
        assert!(entry["error"].is_string(), "skipped carries the reason");
    }
}

#[tokio::test]
async fn deep_health_probes_all_capabilities_through_mock_providers() {
    let server = test_app(
        CacheConfig {
            enabled: true,
            ..Default::default()
        },
        true,
    )
    .await;
    let resp = server.get("/health/deep").await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();

    assert_eq!(body["status"], "ok");
    for cap in ["llm", "embedding", "search"] {
        let entry = &body["capabilities"][cap];
        assert_eq!(entry["status"], "ok", "capability {}: {:?}", cap, entry);
        assert!(entry["error"].is_null());
    }
    // Caching enabled -> the same cache block as /health rides along.
    assert_eq!(body["cache"]["backend"], "memory");
    assert_eq!(body["cache"]["connected"], true);
}

#[tokio::test]
async fn deep_health_second_call_within_ttl_is_served_cached() {
    let server = test_app(CacheConfig::default(), true).await;
    let first: serde_json::Value = server.get("/health/deep").await.json();
    assert_eq!(first["cached"], false);

    let second: serde_json::Value = server.get("/health/deep").await.json();
    assert_eq!(second["cached"], true, "default TTL is 60s — no re-probe");
    assert_eq!(second["checked_at"], first["checked_at"], "same probe run");
}
