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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

/// Build a full app with caller-controlled `settings` and `search_registry`,
/// everything else defaulted/empty — for the search-engine-inference tests
/// below (issue #2879/#2927), which need to vary `[health]
/// search_probe_engine`, `[routing] default_search_engine`, and exactly which
/// search engines are configured independently of each other, none of which
/// `test_app` above exposes.
async fn test_app_with(settings: Settings, search_registry: SearchRegistry) -> TestServer {
    let db = common::in_memory_db().await;
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let search_registry = Arc::new(search_registry);

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new(HashMap::new())),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(EmbeddingRegistry::new(HashMap::new())),
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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

fn mock_search_registry_health(engines: &[&str]) -> SearchRegistry {
    SearchRegistry::new_with_mock_engines(
        engines
            .iter()
            .map(|&e| {
                let adapter: Arc<dyn modelrouter::providers::search::SearchAdapter> =
                    Arc::new(common::MockSearchAdapter {
                        results: vec![SearchResultItem {
                            title: "t".to_string(),
                            url: "https://example.com".to_string(),
                            snippet: "s".to_string(),
                            score: None,
                            published_date: None,
                        }],
                    });
                (e, adapter)
            })
            .collect(),
    )
}

// ── GET /health/deep search-engine inference (issue #2879/#2927) ───────────────
//
// Mirrors /v1/search's own precedence (api/routes/search.rs::resolve_engine):
// explicit config wins, then [routing] default_search_engine, then the sole
// configured provider, then "cannot determine" rather than a hardcoded guess.

#[tokio::test]
async fn explicit_search_probe_engine_wins_even_when_inference_would_differ() {
    // Two engines configured (so plain inference would be Undetermined) AND a
    // routing default naming the other one -- the explicit probe config must
    // still win over both.
    let settings = Settings {
        routing: modelrouter::config::schema::RoutingConfig {
            default_search_engine: Some("tavily".to_string()),
            ..Default::default()
        },
        health: modelrouter::config::schema::HealthConfig {
            search_probe_engine: Some("vertex".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let server = test_app_with(settings, mock_search_registry_health(&["tavily", "vertex"])).await;
    let body: serde_json::Value = server.get("/health/deep").await.json();
    assert_eq!(body["capabilities"]["search"]["status"], "ok");
    assert_eq!(body["capabilities"]["search"]["target"], "search/vertex");
}

#[tokio::test]
async fn unset_probe_engine_infers_the_sole_configured_provider() {
    // The exact false-alarm this issue reports: only Vertex search is
    // configured, nothing explicit anywhere. The old hardcoded "tavily"
    // default reported this healthy path as down.
    let settings = Settings::default();
    let server = test_app_with(settings, mock_search_registry_health(&["vertex"])).await;
    let body: serde_json::Value = server.get("/health/deep").await.json();
    assert_eq!(
        body["capabilities"]["search"]["status"],
        "ok",
        "{:?}",
        body["capabilities"]["search"]
    );
    assert_eq!(body["capabilities"]["search"]["target"], "search/vertex");
}

#[tokio::test]
async fn unset_probe_engine_falls_back_to_routing_default_search_engine() {
    let settings = Settings {
        routing: modelrouter::config::schema::RoutingConfig {
            default_search_engine: Some("vertex".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let server = test_app_with(settings, mock_search_registry_health(&["tavily", "vertex"])).await;
    let body: serde_json::Value = server.get("/health/deep").await.json();
    assert_eq!(body["capabilities"]["search"]["status"], "ok");
    assert_eq!(body["capabilities"]["search"]["target"], "search/vertex");
}

#[tokio::test]
async fn zero_engines_and_nothing_explicit_reports_undetermined_not_a_guess() {
    let settings = Settings::default();
    let server = test_app_with(settings, mock_search_registry_health(&[])).await;
    let body: serde_json::Value = server.get("/health/deep").await.json();
    let search = &body["capabilities"]["search"];
    assert_eq!(search["status"], "skipped", "{search:?}");
    assert_eq!(search["target"], "search/(undetermined)");
    let reason = search["error"].as_str().unwrap_or_default();
    assert!(reason.contains("no search engine configured"), "{reason}");
}

#[tokio::test]
async fn multiple_engines_and_nothing_explicit_reports_undetermined_not_a_guess() {
    // This is the failure mode that matters most: guessing here would mean
    // the probe silently tests ONE of two real engines and reports the other
    // healthy-by-omission. Must refuse instead, naming both.
    let settings = Settings::default();
    let server = test_app_with(settings, mock_search_registry_health(&["tavily", "vertex"])).await;
    let body: serde_json::Value = server.get("/health/deep").await.json();
    let search = &body["capabilities"]["search"];
    assert_eq!(search["status"], "skipped", "{search:?}");
    assert_eq!(search["target"], "search/(undetermined)");
    let reason = search["error"].as_str().unwrap_or_default();
    assert!(reason.contains("tavily") && reason.contains("vertex"), "{reason}");
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
