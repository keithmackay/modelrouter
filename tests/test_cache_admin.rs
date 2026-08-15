//! Admin API + dashboard coverage for the response cache.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, Settings};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::cache::ResponseCache;
use modelrouter::router::{
    complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

async fn build_server() -> (TestServer, Arc<Settings>, Arc<ResponseCache>) {
    let db = common::in_memory_db().await;
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
            response: "ok".to_string(),
        })),
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
            HashMap::new(),
        )),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            HashMap::new(),
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
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    (
        TestServer::new(build_router(state)).unwrap(),
        settings,
        response_cache,
    )
}

fn jwt(settings: &Settings, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    issue_jwt(
        &AdminClaims {
            sub: 1,
            name: format!("{}-user", role),
            role: role.to_string(),
            exp,
        },
        &settings.auth.jwt_secret,
    )
    .unwrap()
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

#[tokio::test]
async fn stats_requires_authentication() {
    let (server, _settings, _cache) = build_server().await;
    let resp = server.get("/admin/api/cache/stats").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn stats_reports_live_and_ledger_views() {
    let (server, settings, cache) = build_server().await;
    cache
        .put_completion(
            "completion:x:y",
            "gpt-4o",
            &modelrouter::providers::adapter::CompletionResult::default(),
            0.5,
        )
        .await;
    cache.get_completion("completion:x:y", "gpt-4o").await.unwrap();

    let (n, v) = bearer(&jwt(&settings, "viewer"));
    let resp = server.get("/admin/api/cache/stats").add_header(n, v).await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["live"]["backend"], "memory");
    assert_eq!(body["live"]["hits"], 1);
    assert_eq!(body["live"]["entries"], 1);
    assert!((body["live"]["saved_usd"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert!(body["ledger"]["hit_rate"].is_number());
    assert!(body["ledger"]["by_model"].is_array());
}

#[tokio::test]
async fn policy_is_readable_and_updatable() {
    let (server, settings, cache) = build_server().await;
    let (n, v) = bearer(&jwt(&settings, "viewer"));
    let resp = server.get("/admin/api/cache/policy").add_header(n, v).await;
    assert_eq!(resp.status_code(), 200);
    let policy: serde_json::Value = resp.json();
    assert_eq!(policy["completions"]["max_temperature"], 0.0);

    let (n, v) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .put("/admin/api/cache/policy")
        .add_header(n, v)
        .json(&json!({ "completions_max_temperature": 0.4, "search_ttl_seconds": 60 }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let updated: serde_json::Value = resp.json();
    assert_eq!(updated["completions"]["max_temperature"], 0.4);
    assert_eq!(updated["search"]["ttl_seconds"], 60);

    // The change is live, not just reported.
    assert!(cache.completion_eligible(&json!({"temperature": 0.4})));
}

#[tokio::test]
async fn policy_update_requires_superadmin() {
    let (server, settings, _cache) = build_server().await;
    let (n, v) = bearer(&jwt(&settings, "viewer"));
    let resp = server
        .put("/admin/api/cache/policy")
        .add_header(n, v)
        .json(&json!({ "enabled": false }))
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn purge_all_empties_the_cache() {
    let (server, settings, cache) = build_server().await;
    for key in ["completion:a:1", "completion:a:2"] {
        cache
            .put_completion(
                key,
                "gpt-4o",
                &modelrouter::providers::adapter::CompletionResult::default(),
                0.0,
            )
            .await;
    }

    let (n, v) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/cache/purge")
        .add_header(n, v)
        .json(&json!({ "scope": "all" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["removed"], 2);
    assert!(cache.get_completion("completion:a:1", "gpt-4o").await.is_none());
}

#[tokio::test]
async fn purge_by_model_only_removes_that_model() {
    let (server, settings, cache) = build_server().await;
    let gpt_key = modelrouter::router::cache::completion_cache_key("gpt-4o", &json!({"m": 1}));
    let claude_key = modelrouter::router::cache::completion_cache_key("claude", &json!({"m": 1}));
    for (key, model) in [(&gpt_key, "gpt-4o"), (&claude_key, "claude")] {
        cache
            .put_completion(
                key,
                model,
                &modelrouter::providers::adapter::CompletionResult::default(),
                0.0,
            )
            .await;
    }

    let (n, v) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/cache/purge")
        .add_header(n, v)
        .json(&json!({ "scope": "model", "model": "gpt-4o" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.json::<serde_json::Value>()["removed"], 1);
    assert!(cache.get_completion(&claude_key, "claude").await.is_some());
}

#[tokio::test]
async fn purge_by_model_without_a_model_is_rejected() {
    let (server, settings, _cache) = build_server().await;
    let (n, v) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/cache/purge")
        .add_header(n, v)
        .json(&json!({ "scope": "model" }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn purge_rejects_unknown_scope() {
    let (server, settings, _cache) = build_server().await;
    let (n, v) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/cache/purge")
        .add_header(n, v)
        .json(&json!({ "scope": "everything-ish" }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn dashboard_page_requires_a_session() {
    let (server, _settings, _cache) = build_server().await;
    let resp = server.get("/admin/cache").await;
    assert_eq!(resp.status_code(), 303, "should redirect to the login page");
}

#[tokio::test]
async fn dashboard_page_renders_stats_and_controls() {
    let (server, settings, _cache) = build_server().await;
    let token = jwt(&settings, "superadmin");
    let resp = server
        .get("/admin/cache")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("Response Cache"));
    assert!(html.contains("Hit rate"));
    assert!(html.contains("/admin/cache/purge"));
    assert!(html.contains("/admin/cache/policy"));
}
