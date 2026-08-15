//! Runtime model-alias administration (issue #9):
//! admin API CRUD, no-restart effect on routing, DB-beats-config precedence,
//! cycle rejection, audit visibility and admin auth.

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

/// Build a server whose config already defines `deep -> openai/gpt-4o`,
/// so DB-over-config precedence is observable.
async fn build_server() -> (TestServer, Arc<Settings>, Arc<RequestRouter>, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    // Audit rows reference admin_users(id); seed the actor the test JWTs claim.
    {
        use modelrouter::db::models::NewAdminUser;
        use modelrouter::db::repositories::admin_users::AdminUserRepository;
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
    }
    let mut base = Settings::default();
    base.routing
        .model_aliases
        .insert("deep".to_string(), "openai/gpt-4o".to_string());
    let settings = Arc::new(base);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: router.clone(),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "ok".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
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
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    (
        TestServer::new(build_router(state)).unwrap(),
        settings,
        router,
        db,
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
async fn alias_crud_round_trip() {
    let (server, settings, _router, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    // Create
    let res = server
        .put("/admin/api/aliases/balanced")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "anthropic/claude-sonnet-4-5" }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<serde_json::Value>()["target"], "anthropic/claude-sonnet-4-5");

    // List
    let listed = server
        .get("/admin/api/aliases")
        .add_header(hk.clone(), hv.clone())
        .await
        .json::<serde_json::Value>();
    assert_eq!(listed["aliases"].as_array().unwrap().len(), 1);
    assert_eq!(listed["effective"]["balanced"], "anthropic/claude-sonnet-4-5");

    // Update (upsert must replace, not duplicate)
    server
        .put("/admin/api/aliases/balanced")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "openai/gpt-5-mini" }))
        .await
        .assert_status_ok();
    let listed = server
        .get("/admin/api/aliases")
        .add_header(hk.clone(), hv.clone())
        .await
        .json::<serde_json::Value>();
    assert_eq!(listed["aliases"].as_array().unwrap().len(), 1);
    assert_eq!(listed["effective"]["balanced"], "openai/gpt-5-mini");

    // Delete
    server
        .delete("/admin/api/aliases/balanced")
        .add_header(hk.clone(), hv.clone())
        .await
        .assert_status_ok();
    let listed = server
        .get("/admin/api/aliases")
        .add_header(hk.clone(), hv.clone())
        .await
        .json::<serde_json::Value>();
    assert!(listed["aliases"].as_array().unwrap().is_empty());

    // Deleting again is a clear 4xx, not a silent success.
    server
        .delete("/admin/api/aliases/balanced")
        .add_header(hk, hv)
        .await
        .assert_status_bad_request();
}

#[tokio::test]
async fn alias_write_takes_effect_without_restart_and_beats_config() {
    let (server, settings, router, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    // Config alias is what resolves before any runtime alias exists.
    assert_eq!(
        router.resolve("deep"),
        ("openai".to_string(), "gpt-4o".to_string())
    );

    server
        .put("/admin/api/aliases/deep")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "anthropic/claude-opus-4-6" }))
        .await
        .assert_status_ok();

    // Same process, no restart: the DB alias now wins.
    assert_eq!(
        router.resolve("deep"),
        ("anthropic".to_string(), "claude-opus-4-6".to_string())
    );

    // Deleting the runtime alias restores the config alias, still live.
    server
        .delete("/admin/api/aliases/deep")
        .add_header(hk, hv)
        .await
        .assert_status_ok();
    assert_eq!(
        router.resolve("deep"),
        ("openai".to_string(), "gpt-4o".to_string())
    );
}

#[tokio::test]
async fn alias_chain_resolves_and_cycles_are_rejected() {
    let (server, settings, router, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    // A chain of aliases is legal.
    for (alias, target) in [("tier1", "tier2"), ("tier2", "anthropic/claude-opus-4-6")] {
        server
            .put(&format!("/admin/api/aliases/{alias}"))
            .add_header(hk.clone(), hv.clone())
            .json(&json!({ "target": target }))
            .await
            .assert_status_ok();
    }
    assert_eq!(
        router.resolve("tier1"),
        ("anthropic".to_string(), "claude-opus-4-6".to_string())
    );

    // Closing the loop is rejected at write time...
    let res = server
        .put("/admin/api/aliases/tier2")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "tier1" }))
        .await;
    res.assert_status_bad_request();
    assert!(res.text().contains("cycle"));

    // ...and a self-referential alias too.
    server
        .put("/admin/api/aliases/selfref")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "selfref" }))
        .await
        .assert_status_bad_request();

    // The rejected writes did not corrupt the live map.
    assert_eq!(
        router.resolve("tier1"),
        ("anthropic".to_string(), "claude-opus-4-6".to_string())
    );
}

#[tokio::test]
async fn routing_shortcut_prefix_is_reserved() {
    let (server, settings, _router, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    server
        .put("/admin/api/aliases/:fastest")
        .add_header(hk, hv)
        .json(&json!({ "target": "openai/gpt-5" }))
        .await
        .assert_status_bad_request();
}

#[tokio::test]
async fn alias_writes_are_audited() {
    let (server, settings, _router, db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    server
        .put("/admin/api/aliases/deep")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "anthropic/claude-opus-4-6" }))
        .await
        .assert_status_ok();
    server
        .put("/admin/api/aliases/deep")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "openai/gpt-5" }))
        .await
        .assert_status_ok();
    server
        .delete("/admin/api/aliases/deep")
        .add_header(hk, hv)
        .await
        .assert_status_ok();

    use modelrouter::db::repositories::audit::AuditRepository;
    let entries = AuditRepository::list(&*db, 50, 0).await.unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"alias.create"), "actions: {actions:?}");
    assert!(actions.contains(&"alias.update"), "actions: {actions:?}");
    assert!(actions.contains(&"alias.delete"), "actions: {actions:?}");

    let update = entries.iter().find(|e| e.action == "alias.update").unwrap();
    assert_eq!(update.target.as_deref(), Some("alias:deep"));
    assert!(update.actor_name.contains("superadmin"));
    assert!(update.before_json.as_ref().unwrap().contains("claude-opus-4-6"));
    assert!(update.after_json.as_ref().unwrap().contains("gpt-5"));
}

#[tokio::test]
async fn alias_endpoints_require_admin_auth() {
    let (server, settings, _router, _db) = build_server().await;

    // No token at all.
    server.get("/admin/api/aliases").await.assert_status_unauthorized();
    server
        .put("/admin/api/aliases/deep")
        .json(&json!({ "target": "openai/gpt-5" }))
        .await
        .assert_status_unauthorized();
    server
        .delete("/admin/api/aliases/deep")
        .await
        .assert_status_unauthorized();

    // A viewer may read but not write.
    let (hk, hv) = bearer(&jwt(&settings, "viewer"));
    server
        .get("/admin/api/aliases")
        .add_header(hk.clone(), hv.clone())
        .await
        .assert_status_ok();
    server
        .put("/admin/api/aliases/deep")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "openai/gpt-5" }))
        .await
        .assert_status_forbidden();
    server
        .delete("/admin/api/aliases/deep")
        .add_header(hk, hv)
        .await
        .assert_status_forbidden();
}

#[tokio::test]
async fn runtime_alias_overrides_model_row_alias() {
    let (server, settings, router, db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    use modelrouter::db::models::NewModel;
    use modelrouter::db::repositories::models::ModelRepository;
    db.create_model(NewModel {
        provider: "openai".to_string(),
        name: "gpt-5-mini".to_string(),
        alias: Some("quick".to_string()),
    })
    .await
    .unwrap();

    // Model-row alias wins over config once loaded.
    server
        .put("/admin/api/aliases/unrelated")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "openai/gpt-5" }))
        .await
        .assert_status_ok();
    assert_eq!(
        router.resolve("quick"),
        ("openai".to_string(), "gpt-5-mini".to_string())
    );

    // A runtime alias with the same name overrides the model row.
    server
        .put("/admin/api/aliases/quick")
        .add_header(hk, hv)
        .json(&json!({ "target": "anthropic/claude-haiku-4-5" }))
        .await
        .assert_status_ok();
    assert_eq!(
        router.resolve("quick"),
        ("anthropic".to_string(), "claude-haiku-4-5".to_string())
    );
}

/// /v1/models advertises routing aliases — config and DB — with `alias_for`,
/// so alias-only deployments no longer return an empty model list (issue #25).
#[tokio::test]
async fn v1_models_lists_config_and_db_aliases() {
    let (server, _settings, router, _db) = build_server().await;
    router.update_db_aliases(HashMap::from([(
        "quick".to_string(),
        "openai/gpt-4o-mini".to_string(),
    )]));

    let body: serde_json::Value = server.get("/v1/models").await.json();
    let data = body["data"].as_array().expect("data array");
    let find = |id: &str| data.iter().find(|m| m["id"] == id).cloned();

    let deep = find("deep").expect("config alias listed");
    assert_eq!(deep["alias_for"], "openai/gpt-4o");
    assert_eq!(deep["owned_by"], "openai");

    let quick = find("quick").expect("db alias listed");
    assert_eq!(quick["alias_for"], "openai/gpt-4o-mini");
    assert_eq!(quick["owned_by"], "openai");
}
