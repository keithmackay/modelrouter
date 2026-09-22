//! HTTP handler tests for /v1/mcp/* endpoints.

mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::Settings;
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::sqlite::SqliteDb;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app_with_auth() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    (build_mcp_app(db.clone()), db)
}

/// Like `test_app_with_auth`, but also hands back the concrete SQLite handle.
/// Cloning shares the underlying pool, so raw DDL run through it is visible to
/// the live server — which is how a test can break the schema underneath it.
async fn test_app_with_sqlite() -> (TestServer, SqliteDb) {
    let sqlite = common::in_memory_db().await;
    common::create_user(&sqlite, "test-user", "test-token").await;
    let db: Arc<dyn DatabaseProvider> = Arc::new(sqlite.clone());
    (build_mcp_app(db), sqlite)
}

fn build_mcp_app(db: Arc<dyn DatabaseProvider>) -> TestServer {
    let settings = Arc::new(Settings::default());
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2, 0.3, 0.4] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

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
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
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
    TestServer::new(build_router(state)).unwrap()
}

async fn test_app_no_embedding() -> TestServer {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new(HashMap::new()),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

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
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
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
    TestServer::new(build_router(state)).unwrap()
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

#[tokio::test]
async fn list_mcp_servers_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server.get("/v1/mcp/servers").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn list_mcp_servers_returns_empty_list() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .get("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["servers"], serde_json::json!([]));
}

#[tokio::test]
async fn list_mcp_servers_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .get("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn create_mcp_server_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/mcp/servers")
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com",
            "description": "Test server"
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn create_mcp_server_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn create_mcp_server_succeeds() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com",
            "description": "Test server"
        }))
        .await;
    assert_eq!(resp.status_code(), 201);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["name"], "test-server");
    assert_eq!(body["url"], "https://example.com");
    assert_eq!(body["description"], "Test server");
}

#[tokio::test]
async fn create_mcp_server_duplicate_name_fails() {
    let (server, _db) = test_app_with_auth().await;
    server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;

    let resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://other.com"
        }))
        .await;
    assert_eq!(resp.status_code(), 409);
}

#[tokio::test]
async fn get_mcp_server_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server.get("/v1/mcp/servers/1").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn get_mcp_server_not_found() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .get("/v1/mcp/servers/999")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn get_mcp_server_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .get("/v1/mcp/servers/1")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn get_mcp_server_succeeds() {
    let (server, _db) = test_app_with_auth().await;
    let create_resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;
    let created: serde_json::Value = create_resp.json();
    let id = created["id"].as_i64().unwrap();

    let resp = server
        .get(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["name"], "test-server");
}

#[tokio::test]
async fn update_mcp_server_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .patch("/v1/mcp/servers/1")
        .json(&serde_json::json!({
            "name": "updated"
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn update_mcp_server_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .patch("/v1/mcp/servers/1")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .json(&serde_json::json!({"name": "updated"}))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn update_mcp_server_not_found() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .patch("/v1/mcp/servers/999")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"name": "updated"}))
        .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn update_mcp_server_not_owner() {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    UserRepository::create(&db, NewUser {
        name: "other-user".to_string(),
        email: None,
    })
    .await
    .unwrap();
    let other_user = UserRepository::find_by_name(&db, "other-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(&db, NewApiKey {
        user_id: other_user.id,
        key_hash: hash_token("other-token"),
        label: Some("other".to_string()),
        expires_at: None,
        project: None,
        session_window_secs: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter { response: "ok".to_string() })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(&modelrouter::config::schema::CacheConfig::default())),
        embedding_registry: Arc::new(modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2, 0.3, 0.4] })),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(std::collections::HashMap::new())),
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
    let server = TestServer::new(build_router(state)).unwrap();

    let create_resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;
    let created: serde_json::Value = create_resp.json();
    let id = created["id"].as_i64().unwrap();

    let resp = server
        .patch(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("other-token").0, bearer("other-token").1)
        .json(&serde_json::json!({"name": "hacked"}))
        .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn update_mcp_server_succeeds() {
    let (server, _db) = test_app_with_auth().await;
    let create_resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;
    let created: serde_json::Value = create_resp.json();
    let id = created["id"].as_i64().unwrap();

    let resp = server
        .patch(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "updated-server",
            "url": "https://updated.com",
            "description": "Updated",
            "enabled": false
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["name"], "updated-server");
    assert_eq!(body["url"], "https://updated.com");
    assert_eq!(body["description"], "Updated");
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn delete_mcp_server_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server.delete("/v1/mcp/servers/1").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn delete_mcp_server_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .delete("/v1/mcp/servers/1")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn delete_mcp_server_not_found() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .delete("/v1/mcp/servers/999")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn delete_mcp_server_not_owner() {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    UserRepository::create(&db, NewUser {
        name: "other-user".to_string(),
        email: None,
    })
    .await
    .unwrap();
    let other_user = UserRepository::find_by_name(&db, "other-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(&db, NewApiKey {
        user_id: other_user.id,
        key_hash: hash_token("other-token"),
        label: Some("other".to_string()),
        expires_at: None,
        project: None,
        session_window_secs: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter { response: "ok".to_string() })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(&modelrouter::config::schema::CacheConfig::default())),
        embedding_registry: Arc::new(modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2, 0.3, 0.4] })),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(std::collections::HashMap::new())),
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
    let server = TestServer::new(build_router(state)).unwrap();

    let create_resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;
    let created: serde_json::Value = create_resp.json();
    let id = created["id"].as_i64().unwrap();

    let resp = server
        .delete(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("other-token").0, bearer("other-token").1)
        .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn delete_mcp_server_succeeds() {
    let (server, _db) = test_app_with_auth().await;
    let create_resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;
    let created: serde_json::Value = create_resp.json();
    let id = created["id"].as_i64().unwrap();

    let resp = server
        .delete(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_eq!(resp.status_code(), 204);

    let get_resp = server
        .get(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_eq!(get_resp.status_code(), 404);
}

#[tokio::test]
async fn discover_mcp_tools_requires_auth() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/mcp/discover")
        .json(&serde_json::json!({
            "prompt": "find a tool"
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn discover_mcp_tools_rejects_experiment_header() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .add_header(
            axum::http::HeaderName::from_static("x-modelrouter-experiment"),
            axum::http::HeaderValue::from_static("exp-1"),
        )
        .json(&serde_json::json!({"prompt": "test"}))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn discover_mcp_tools_empty_servers() {
    let (server, _db) = test_app_with_auth().await;
    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "prompt": "find a tool"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["results"], serde_json::json!([]));
}

#[tokio::test]
async fn discover_mcp_tools_no_embedding_provider() {
    let server = test_app_no_embedding().await;
    server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "test-server",
            "url": "https://example.com"
        }))
        .await;

    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "prompt": "find a tool"
        }))
        .await;
    assert_eq!(resp.status_code(), 503);
}

#[tokio::test]
async fn discover_mcp_tools_succeeds() {
    let (server, _db) = test_app_with_auth().await;
    server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "server-1",
            "url": "https://s1.com",
            "description": "File system tools"
        }))
        .await;

    server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "server-2",
            "url": "https://s2.com",
            "description": "Database tools"
        }))
        .await;

    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "prompt": "file operations",
            "top_k": 5
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert!(body["results"].is_array());
    assert!(body["results"].as_array().unwrap().len() > 0);
}

#[tokio::test]
async fn discover_mcp_tools_only_enabled_servers() {
    let (server, _db) = test_app_with_auth().await;
    let create_resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "disabled-server",
            "url": "https://example.com"
        }))
        .await;
    let created: serde_json::Value = create_resp.json();
    let id = created["id"].as_i64().unwrap();

    server
        .patch(&format!("/v1/mcp/servers/{}", id))
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"enabled": false}))
        .await;

    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"prompt": "test"}))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["results"], serde_json::json!([]));
}

#[tokio::test]
async fn discover_mcp_tools_uses_default_top_k() {
    let (server, _db) = test_app_with_auth().await;
    for i in 0..10 {
        server
            .post("/v1/mcp/servers")
            .add_header(bearer("test-token").0, bearer("test-token").1)
            .json(&serde_json::json!({
                "name": format!("server-{}", i),
                "url": format!("https://s{}.com", i)
            }))
            .await;
    }

    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"prompt": "test"}))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["results"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn discover_mcp_tools_respects_top_k() {
    let (server, _db) = test_app_with_auth().await;
    for i in 0..10 {
        server
            .post("/v1/mcp/servers")
            .add_header(bearer("test-token").0, bearer("test-token").1)
            .json(&serde_json::json!({
                "name": format!("server-{}", i),
                "url": format!("https://s{}.com", i)
            }))
            .await;
    }

    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"prompt": "test", "top_k": 3}))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["results"].as_array().unwrap().len(), 3);
}

// Every MCP handler funnels a repository failure into the same 500 + opaque
// `{"error": "internal error"}` body, so the storage layer's messages never
// leak to callers. Dropping the table under a live server is the cheapest way
// to make each of those arms actually run.
async fn app_with_dropped_mcp_table() -> TestServer {
    let (server, sqlite) = test_app_with_sqlite().await;
    sqlx::query("DROP TABLE mcp_servers")
        .execute(&sqlite.pool)
        .await
        .expect("mcp_servers table can be dropped");
    server
}

/// The 500 body must be the opaque one, never the underlying SQL error.
fn assert_opaque_internal_error(resp: &axum_test::TestResponse) {
    assert_eq!(resp.status_code(), 500);
    assert_eq!(
        resp.json::<serde_json::Value>()["error"],
        "internal error",
        "storage-layer detail leaked to the caller"
    );
}

#[tokio::test]
async fn list_mcp_servers_storage_failure_returns_opaque_500() {
    let server = app_with_dropped_mcp_table().await;
    let resp = server
        .get("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_opaque_internal_error(&resp);
}

#[tokio::test]
async fn create_mcp_server_storage_failure_returns_opaque_500() {
    // A missing table is not a uniqueness violation, so this must fall through
    // the 409 branch to the generic error arm.
    let server = app_with_dropped_mcp_table().await;
    let resp = server
        .post("/v1/mcp/servers")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({
            "name": "weather",
            "url": "https://mcp.example/weather"
        }))
        .await;
    assert_opaque_internal_error(&resp);
}

#[tokio::test]
async fn get_mcp_server_storage_failure_returns_opaque_500() {
    let server = app_with_dropped_mcp_table().await;
    let resp = server
        .get("/v1/mcp/servers/1")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_opaque_internal_error(&resp);
}

#[tokio::test]
async fn update_mcp_server_storage_failure_returns_opaque_500() {
    // The ownership pre-check reads the same table, so it is what fails here —
    // and it must fail closed (500), not fall through to the mutation.
    let server = app_with_dropped_mcp_table().await;
    let resp = server
        .patch("/v1/mcp/servers/1")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"enabled": false}))
        .await;
    assert_opaque_internal_error(&resp);
}

#[tokio::test]
async fn delete_mcp_server_storage_failure_returns_opaque_500() {
    let server = app_with_dropped_mcp_table().await;
    let resp = server
        .delete("/v1/mcp/servers/1")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .await;
    assert_opaque_internal_error(&resp);
}

#[tokio::test]
async fn discover_mcp_tools_storage_failure_returns_opaque_500() {
    let server = app_with_dropped_mcp_table().await;
    let resp = server
        .post("/v1/mcp/discover")
        .add_header(bearer("test-token").0, bearer("test-token").1)
        .json(&serde_json::json!({"prompt": "what is the weather"}))
        .await;
    assert_opaque_internal_error(&resp);
}
