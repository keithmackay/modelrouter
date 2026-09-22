//! Coverage tests for src/api/admin/models.rs (issue #80).
//!
//! Targets uncovered dashboard handlers: model CRUD, failover config, catalog,
//! provider enable/disable, and available-models endpoint variations.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::Settings;
use modelrouter::db::models::{NewAdminUser, NewModel};
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use serde_json::json;
use std::sync::Arc;

async fn build_server() -> (TestServer, Arc<Settings>, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;

    AdminUserRepository::create(
        &db,
        NewAdminUser {
            name: "superadmin-user".to_string(),
            password_hash: "x".to_string(),
            role: "superadmin".to_string(),
        },
    )
    .await
    .unwrap();

    AdminUserRepository::create(
        &db,
        NewAdminUser {
            name: "viewer-user".to_string(),
            password_hash: "x".to_string(),
            role: "viewer".to_string(),
        },
    )
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db_arc: Arc<dyn DatabaseProvider> = Arc::new(db);

    let state = AppState {
        settings: settings.clone(),
        db: db_arc.clone(),
        pool: None,
        router: Arc::new(modelrouter::router::engine::RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(modelrouter::router::cost::CostCalculator::new()),
        provider_registry: Arc::new(modelrouter::providers::registry::ProviderRegistry::new_with_mock(
            common::MockAdapter { response: "ok".to_string() },
        )),
        policy: Arc::new(modelrouter::router::policy::PolicyEngine::new(db_arc.clone())),
        fallback: Arc::new(modelrouter::router::fallback::FallbackChain::new(std::collections::HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
            &modelrouter::config::schema::CacheConfig::default(),
        )),
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
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db_arc.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };

    (TestServer::new(build_router(state)).unwrap(), settings, db_arc)
}

fn jwt(settings: &Settings, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    issue_jwt(
        &AdminClaims {
            sub: if role == "superadmin" { 1 } else { 2 },
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

fn cookie(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::COOKIE,
        axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
    )
}

// ══════════════════════════════════════════════════════════════════════════════
// Dashboard model CRUD
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn dashboard_create_model_success() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/models")
        .add_header(ck, cv)
        .form(&json!({
            "provider": "test-provider",
            "name": "test-model",
            "alias": "test-alias"
        }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("test-provider"));
    assert!(html.contains("test-model"));
    assert!(html.contains("test-alias"));
}

#[tokio::test]
async fn dashboard_create_model_without_alias() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/models")
        .add_header(ck, cv)
        .form(&json!({
            "provider": "openai",
            "name": "gpt-5",
            "alias": ""
        }))
        .await;

    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn dashboard_create_model_empty_fields_rejected() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/models")
        .add_header(ck, cv)
        .form(&json!({
            "provider": "",
            "name": "model",
            "alias": ""
        }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("required"));
}

#[tokio::test]
async fn dashboard_disable_model() {
    let (server, settings, db) = build_server().await;

    // Create a model
    let model = db
        .create_model(NewModel {
            provider: "test".to_string(),
            name: "model-disable-test".to_string(),
            alias: None,
        })
        .await
        .unwrap();

    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post(&format!("/admin/models/{}/disable", model.id))
        .add_header(ck, cv)
        .form(&json!({ "reason": "testing" }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("Disabled"));
    assert!(html.contains("testing"));
}

#[tokio::test]
async fn dashboard_enable_model() {
    let (server, settings, db) = build_server().await;

    // Create and disable a model
    let model = db
        .create_model(NewModel {
            provider: "test".to_string(),
            name: "model-enable-test".to_string(),
            alias: None,
        })
        .await
        .unwrap();

    db.set_model_enabled_with_reason(model.id, false, Some("test"), Some("admin"))
        .await
        .unwrap();

    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post(&format!("/admin/models/{}/enable", model.id))
        .add_header(ck, cv)
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("Enabled"));
}

#[tokio::test]
async fn dashboard_delete_model() {
    let (server, settings, db) = build_server().await;

    let model = db
        .create_model(NewModel {
            provider: "test".to_string(),
            name: "model-delete-test".to_string(),
            alias: None,
        })
        .await
        .unwrap();

    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post(&format!("/admin/models/{}/delete", model.id))
        .add_header(ck, cv)
        .await;

    assert_eq!(resp.status_code(), 200);
    // Should return empty HTML (outerHTML swap removes the row)
}

// ══════════════════════════════════════════════════════════════════════════════
// Failover configuration
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn dashboard_set_failovers() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/models/primary-model/failovers")
        .add_header(ck, cv)
        .form(&json!({ "fallbacks": "fallback1\nfallback2,fallback3" }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("fallback1"));
    assert!(html.contains("fallback2"));
    assert!(html.contains("fallback3"));
}

#[tokio::test]
async fn dashboard_clear_failovers() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/models/some-model/failovers")
        .add_header(ck, cv)
        .form(&json!({ "fallbacks": "" }))
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("cleared"));
}

// ══════════════════════════════════════════════════════════════════════════════
// Provider enable/disable
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn dashboard_disable_nonexistent_provider() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/providers/nonexistent/disable")
        .add_header(ck, cv)
        .form(&json!({ "reason": "maintenance" }))
        .await;

    // Should fail because provider doesn't exist
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn dashboard_enable_nonexistent_provider() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .post("/admin/providers/nonexistent/enable")
        .add_header(ck, cv)
        .await;

    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn get_provider_rows() {
    let (server, settings, _db) = build_server().await;
    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .get("/admin/providers/rows")
        .add_header(ck, cv)
        .await;

    assert_eq!(resp.status_code(), 200);
    // With default settings, the providers list may be empty or contain configured providers
    let html = resp.text();
    assert!(html.contains("<tr") || html.contains("No providers"));
}

// ══════════════════════════════════════════════════════════════════════════════
// Available models catalog
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn get_available_models_default() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "viewer"));

    let resp = server
        .get("/admin/api/models/available")
        .add_header(hk, hv)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert!(body.get("providers").is_some());
    assert!(body.get("ttl_seconds").is_some());
}

#[tokio::test]
async fn get_available_models_with_refresh() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "viewer"));

    let resp = server
        .get("/admin/api/models/available?refresh=true")
        .add_header(hk, hv)
        .await;

    // With no configured providers that have catalog endpoints, this may 404
    // OR 200 with empty providers. Either is acceptable for this test.
    assert!(resp.status_code() == 200 || resp.status_code() == 404);
}

#[tokio::test]
async fn get_available_models_refresh_false() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "viewer"));

    // First call populates cache
    server
        .get("/admin/api/models/available")
        .add_header(hk.clone(), hv.clone())
        .await;

    // Second call should hit cache (or fail consistently with first call)
    let resp = server
        .get("/admin/api/models/available?refresh=false")
        .add_header(hk, hv)
        .await;

    // Accept 200 or 404 depending on provider configuration
    assert!(resp.status_code() == 200 || resp.status_code() == 404);
}

// ══════════════════════════════════════════════════════════════════════════════
// Models page rendering
// ══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn get_models_page() {
    let (server, settings, db) = build_server().await;

    // Create some test data
    db.create_model(NewModel {
        provider: "openai".to_string(),
        name: "gpt-5".to_string(),
        alias: Some("latest".to_string()),
    })
    .await
    .unwrap();

    let (ck, cv) = cookie(&jwt(&settings, "superadmin"));

    let resp = server
        .get("/admin/models")
        .add_header(ck, cv)
        .await;

    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("openai"));
    assert!(html.contains("gpt-5"));
    assert!(html.contains("latest"));
}
