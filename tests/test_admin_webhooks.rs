//! Admin webhook management (REST API + dashboard) coverage.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::Settings;
use modelrouter::db::models::NewAdminUser;
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use modelrouter::db::repositories::webhook_callbacks::NewWebhookCallback;
use serde_json::{json, Value};
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
            name: "admin-user".to_string(),
            password_hash: "x".to_string(),
            role: "viewer".to_string(),
        },
    )
    .await
    .unwrap();
    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(modelrouter::router::engine::RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(modelrouter::router::cost::CostCalculator::new()),
        provider_registry: Arc::new(modelrouter::providers::registry::ProviderRegistry::new_with_mock(
            common::MockAdapter { response: "ok".to_string() },
        )),
        policy: Arc::new(modelrouter::router::policy::PolicyEngine::new(db.clone())),
        fallback: Arc::new(modelrouter::router::fallback::FallbackChain::new(std::collections::HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(&Default::default())),
        embedding_registry: Arc::new(
            modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
            ),
        ),
        search_registry: Arc::new(
            modelrouter::providers::search_registry::SearchRegistry::new_with_mock(
                common::MockSearchAdapter { results: vec![] },
            ),
        ),
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
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    (TestServer::new(build_router(state)).unwrap(), settings, db)
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

// ── REST API ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_webhooks_api_requires_auth() {
    let (server, _settings, _db) = build_server().await;
    let resp = server.get("/admin/api/webhooks").await;
    assert_eq!(resp.status_code(), 303);
}

#[tokio::test]
async fn list_webhooks_api_admin_can_read() {
    let (server, settings, db) = build_server().await;
    db.create_webhook(NewWebhookCallback {
        name: "hook1".to_string(),
        url: "https://example.com/hook".to_string(),
        events: r#"["completion"]"#.to_string(),
        secret_header_name: None,
        secret_header_value: None,
    })
    .await
    .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/webhooks").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let webhooks: Value = resp.json();
    assert_eq!(webhooks.as_array().unwrap().len(), 1);
    assert_eq!(webhooks[0]["name"], "hook1");
}

#[tokio::test]
async fn create_webhook_api_requires_superadmin() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/api/webhooks")
        .add_header(hk, hv)
        .json(&json!({
            "name": "hook2",
            "url": "https://example.com/hook2",
        }))
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn create_webhook_api_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/webhooks")
        .add_header(hk, hv)
        .json(&json!({
            "name": "hook3",
            "url": "https://example.com/hook3",
            "events": r#"["completion"]"#,
            "secret_header_name": "X-Secret",
            "secret_header_value": "abc123",
        }))
        .await;
    assert_eq!(resp.status_code(), 201);
    let webhook: Value = resp.json();
    assert_eq!(webhook["name"], "hook3");
    assert_eq!(webhook["url"], "https://example.com/hook3");
    assert_eq!(webhook["secret_header_name"], "X-Secret");
    assert_eq!(webhook["enabled"], true);

    let webhooks = db.list_webhooks().await.unwrap();
    assert_eq!(webhooks.len(), 1);
}

#[tokio::test]
async fn create_webhook_api_defaults_events() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/webhooks")
        .add_header(hk, hv)
        .json(&json!({
            "name": "hook4",
            "url": "https://example.com/hook4",
        }))
        .await;
    assert_eq!(resp.status_code(), 201);

    let webhooks = db.list_webhooks().await.unwrap();
    assert_eq!(webhooks[0].events, r#"["completion"]"#);
}

#[tokio::test]
async fn delete_webhook_api_success() {
    let (server, settings, db) = build_server().await;
    let webhook = db
        .create_webhook(NewWebhookCallback {
            name: "hook5".to_string(),
            url: "https://example.com/hook5".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .delete(&format!("/admin/api/webhooks/{}", webhook.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 204);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(webhooks.is_empty());
}

#[tokio::test]
async fn enable_webhook_api_success() {
    let (server, settings, db) = build_server().await;
    let webhook = db
        .create_webhook(NewWebhookCallback {
            name: "hook6".to_string(),
            url: "https://example.com/hook6".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        })
        .await
        .unwrap();
    db.set_webhook_enabled(webhook.id, false).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/api/webhooks/{}/enable", webhook.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 204);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(webhooks[0].enabled);
}

#[tokio::test]
async fn disable_webhook_api_success() {
    let (server, settings, db) = build_server().await;
    let webhook = db
        .create_webhook(NewWebhookCallback {
            name: "hook7".to_string(),
            url: "https://example.com/hook7".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/api/webhooks/{}/disable", webhook.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 204);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(!webhooks[0].enabled);
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_webhooks_page_requires_session() {
    let (server, _settings, _db) = build_server().await;
    let resp = server.get("/admin/webhooks").await;
    assert_eq!(resp.status_code(), 303);
}

#[tokio::test]
async fn get_webhooks_page_renders_list() {
    let (server, settings, db) = build_server().await;
    db.create_webhook(NewWebhookCallback {
        name: "hook8".to_string(),
        url: "https://example.com/hook8".to_string(),
        events: r#"["completion"]"#.to_string(),
        secret_header_name: Some("X-Secret".to_string()),
        secret_header_value: Some("secret".to_string()),
    })
    .await
    .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server.get("/admin/webhooks").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("hook8"));
    assert!(html.contains("Webhook Callbacks"));
}

#[tokio::test]
async fn post_create_webhook_page_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/webhooks")
        .add_header(hk, hv)
        .form(&[
            ("name", "hook9"),
            ("url", "https://example.com/hook9"),
            ("events", "completion"),
        ])
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert_eq!(webhooks.len(), 1);
    assert_eq!(webhooks[0].name, "hook9");
}

#[tokio::test]
async fn post_create_webhook_page_parses_comma_events() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/webhooks")
        .add_header(hk, hv)
        .form(&[
            ("name", "hook10"),
            ("url", "https://example.com/hook10"),
            ("events", "completion, error"),
        ])
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert_eq!(webhooks[0].events, r#"["completion","error"]"#);
}

#[tokio::test]
async fn post_create_webhook_page_accepts_json_events() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/webhooks")
        .add_header(hk, hv)
        .form(&[
            ("name", "hook11"),
            ("url", "https://example.com/hook11"),
            ("events", r#"["completion"]"#),
        ])
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert_eq!(webhooks[0].events, r#"["completion"]"#);
}

#[tokio::test]
async fn post_create_webhook_page_empty_name_redirects() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/webhooks")
        .add_header(hk, hv)
        .form(&[("name", ""), ("url", "https://example.com/hook")])
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(webhooks.is_empty());
}

#[tokio::test]
async fn post_delete_webhook_page_success() {
    let (server, settings, db) = build_server().await;
    let webhook = db
        .create_webhook(NewWebhookCallback {
            name: "hook12".to_string(),
            url: "https://example.com/hook12".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post(&format!("/admin/webhooks/{}/delete", webhook.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(webhooks.is_empty());
}

#[tokio::test]
async fn post_enable_webhook_page_success() {
    let (server, settings, db) = build_server().await;
    let webhook = db
        .create_webhook(NewWebhookCallback {
            name: "hook13".to_string(),
            url: "https://example.com/hook13".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        })
        .await
        .unwrap();
    db.set_webhook_enabled(webhook.id, false).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post(&format!("/admin/webhooks/{}/enable", webhook.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(webhooks[0].enabled);
}

#[tokio::test]
async fn post_disable_webhook_page_success() {
    let (server, settings, db) = build_server().await;
    let webhook = db
        .create_webhook(NewWebhookCallback {
            name: "hook14".to_string(),
            url: "https://example.com/hook14".to_string(),
            events: r#"["completion"]"#.to_string(),
            secret_header_name: None,
            secret_header_value: None,
        })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post(&format!("/admin/webhooks/{}/disable", webhook.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 303);

    let webhooks = db.list_webhooks().await.unwrap();
    assert!(!webhooks[0].enabled);
}

#[tokio::test]
async fn create_webhook_api_hides_secret_value() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/webhooks")
        .add_header(hk, hv)
        .json(&json!({
            "name": "hook15",
            "url": "https://example.com/hook15",
            "secret_header_name": "X-Secret",
            "secret_header_value": "secret123",
        }))
        .await;
    assert_eq!(resp.status_code(), 201);
    let webhook: Value = resp.json();
    assert_eq!(webhook["secret_header_name"], "X-Secret");
    assert!(webhook.get("secret_header_value").is_none());
}
