//! Operator disable / re-enable of models and providers (issue #5).
//!
//! Covers: exclusion from routing, `/v1/models`, load-balancer pools and alias
//! targets; a clear 4xx (not a provider error) on a direct request; re-enable
//! restoring service; persistence across a restart; audit visibility; admin auth;
//! and that a disable is distinct from a circuit-breaker trip.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{
    CacheConfig, LbPoolEntry, LbStrategy, LoadBalancerConfig, ProviderConfig, Settings,
};
use modelrouter::db::models::NewModel;
use modelrouter::db::repositories::models::ModelRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::cache::ResponseCache;
use modelrouter::router::{
    complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, load_balancer::LoadBalancer, policy::PolicyEngine,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const API_KEY: &str = "mr-test-disable";

fn settings_with_providers() -> Settings {
    let mut s = Settings::default();
    for name in ["openai", "anthropic"] {
        s.providers.insert(
            name.to_string(),
            ProviderConfig {
                api_key: "test".to_string(),
                api_base: Some("http://mock".to_string()),
                timeout_secs: 10,
                api_version: None,
                ..Default::default()
            },
        );
    }
    // A pool with two members so we can disable one and still route.
    s.routing.load_balancer.insert(
        "pool".to_string(),
        LoadBalancerConfig {
            strategy: LbStrategy::RoundRobin,
            pool: vec![
                LbPoolEntry {
                    provider: "openai".to_string(),
                    model: "gpt-5".to_string(),
                    weight: 1,
                },
                LbPoolEntry {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-4-6".to_string(),
                    weight: 1,
                },
            ],
        },
    );
    s
}

/// Build a server on top of an existing database, so a test can rebuild the
/// process (fresh router, fresh in-memory state) against the same rows.
async fn server_from_db(db: Arc<dyn DatabaseProvider>, settings: Arc<Settings>) -> (TestServer, Arc<RequestRouter>) {
    let router = Arc::new(RequestRouter::new(settings.clone()));

    // Same seeding the real `serve` path performs at startup.
    router.update_db_aliases(modelrouter::api::admin::aliases::build_db_alias_map(&db).await);
    router.update_availability(modelrouter::api::admin::aliases::build_availability_map(&db).await);

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
        load_balancer: Arc::new(LoadBalancer::new(settings.routing.load_balancer.clone())),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    (TestServer::new(build_router(state)).unwrap(), router)
}

async fn build_server() -> (TestServer, Arc<Settings>, Arc<RequestRouter>, Arc<dyn DatabaseProvider>) {
    let sqlite = common::in_memory_db().await;
    {
        use modelrouter::db::models::{NewAdminUser, NewUser};
        use modelrouter::db::repositories::admin_users::AdminUserRepository;
        use modelrouter::db::repositories::api_keys::ApiKeyRepository;
        use modelrouter::db::repositories::users::UserRepository;

        AdminUserRepository::create(
            &sqlite,
            NewAdminUser {
                name: "superadmin-user".to_string(),
                password_hash: "x".to_string(),
                role: "superadmin".to_string(),
            },
        )
        .await
        .unwrap();

        let user = UserRepository::create(
            &sqlite,
            NewUser { name: "caller".to_string(), email: None },
        )
        .await
        .unwrap();
        ApiKeyRepository::create_api_key(
            &sqlite,
            modelrouter::db::models::NewApiKey {
                user_id: user.id,
                key_hash: modelrouter::api::auth::hash_token(API_KEY),
                label: None,
                expires_at: None,
                project: None,
                session_window_secs: None,
            },
        )
        .await
        .unwrap();
    }

    let settings = Arc::new(settings_with_providers());
    let db: Arc<dyn DatabaseProvider> = Arc::new(sqlite);
    let (server, router) = server_from_db(db.clone(), settings.clone()).await;
    (server, settings, router, db)
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

fn caller() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", API_KEY)).unwrap(),
    )
}

async fn seed_model(db: &Arc<dyn DatabaseProvider>, provider: &str, name: &str) -> i64 {
    db.create_model(NewModel {
        provider: provider.to_string(),
        name: name.to_string(),
        alias: None,
    })
    .await
    .unwrap()
    .id
}

async fn disable_model(server: &TestServer, settings: &Settings, id: i64, reason: &str) {
    let (hk, hv) = bearer(&jwt(settings, "superadmin"));
    server
        .patch(&format!("/admin/api/models/{id}/enabled"))
        .add_header(hk, hv)
        .json(&json!({ "enabled": false, "reason": reason }))
        .await
        .assert_status_ok();
}

async fn chat(server: &TestServer, model: &str) -> axum_test::TestResponse {
    let (hk, hv) = caller();
    server
        .post("/v1/chat/completions")
        .add_header(hk, hv)
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await
}

#[tokio::test]
async fn disabled_model_is_rejected_with_a_clear_4xx_naming_the_reason() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;

    // Works before the disable.
    chat(&server, "openai/gpt-5").await.assert_status_ok();

    disable_model(&server, &settings, id, "cost spike").await;

    let res = chat(&server, "openai/gpt-5").await;
    assert_eq!(res.status_code(), axum::http::StatusCode::FORBIDDEN);
    let body = res.json::<serde_json::Value>();
    // A 4xx naming the reason — explicitly not a provider_error/502.
    assert_eq!(body["error"]["type"], "model_disabled");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("cost spike"), "{msg}");
    assert!(msg.contains("openai/gpt-5"), "{msg}");
    assert!(msg.contains("superadmin-user"), "{msg}");

    // A sibling model on the same provider still routes.
    chat(&server, "openai/gpt-5-mini").await.assert_status_ok();
}

#[tokio::test]
async fn disable_and_re_enable_take_effect_without_a_restart() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    disable_model(&server, &settings, id, "vulnerability").await;
    assert_eq!(
        chat(&server, "openai/gpt-5").await.status_code(),
        axum::http::StatusCode::FORBIDDEN
    );

    // Same process: re-enabling restores service immediately.
    server
        .patch(&format!("/admin/api/models/{id}/enabled"))
        .add_header(hk, hv)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status_ok();
    chat(&server, "openai/gpt-5").await.assert_status_ok();

    // Re-enabling clears the recorded reason.
    let model = db.get_model(id).await.unwrap().unwrap();
    assert!(model.enabled);
    assert_eq!(model.disabled_reason, None);
    assert_eq!(model.disabled_by, None);
    assert_eq!(model.disabled_at, None);
}

#[tokio::test]
async fn disable_survives_a_restart() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    disable_model(&server, &settings, id, "vendor incident").await;
    drop(server);

    // Rebuild the process against the same database — nothing in memory carries over.
    let (fresh, fresh_router) = server_from_db(db.clone(), settings.clone()).await;
    assert!(!fresh_router.is_available("openai", "gpt-5"));
    let res = chat(&fresh, "openai/gpt-5").await;
    assert_eq!(res.status_code(), axum::http::StatusCode::FORBIDDEN);
    assert!(res.text().contains("vendor incident"));
}

#[tokio::test]
async fn disabling_a_provider_disables_all_of_its_models() {
    let (server, settings, router, db) = build_server().await;
    seed_model(&db, "anthropic", "claude-opus-4-6").await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    server
        .patch("/admin/api/providers/anthropic/enabled")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "enabled": false, "reason": "contract paused" }))
        .await
        .assert_status_ok();

    assert!(!router.is_available("anthropic", "claude-opus-4-6"));
    assert!(!router.is_available("anthropic", "claude-haiku-4-5"));
    assert!(router.is_available("openai", "gpt-5"));

    let res = chat(&server, "anthropic/claude-opus-4-6").await;
    assert_eq!(res.status_code(), axum::http::StatusCode::FORBIDDEN);
    assert!(res.text().contains("contract paused"));

    // Re-enable restores the whole provider.
    server
        .patch("/admin/api/providers/anthropic/enabled")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "enabled": true }))
        .await
        .assert_status_ok();
    assert!(router.is_available("anthropic", "claude-opus-4-6"));

    // Unknown providers cannot be toggled into a phantom disabled state.
    server
        .patch("/admin/api/providers/nosuchprovider/enabled")
        .add_header(hk, hv)
        .json(&json!({ "enabled": false }))
        .await
        .assert_status_bad_request();
}

#[tokio::test]
async fn disabled_entities_are_excluded_from_v1_models() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    seed_model(&db, "anthropic", "claude-opus-4-6").await;

    let ids = |v: &serde_json::Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect()
    };

    let before = ids(&server.get("/v1/models").await.json());
    assert!(before.contains(&"openai/gpt-5".to_string()));
    assert!(before.contains(&"anthropic".to_string()));

    disable_model(&server, &settings, id, "deprecated").await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    server
        .patch("/admin/api/providers/anthropic/enabled")
        .add_header(hk, hv)
        .json(&json!({ "enabled": false, "reason": "paused" }))
        .await
        .assert_status_ok();

    let after = ids(&server.get("/v1/models").await.json());
    assert!(!after.contains(&"openai/gpt-5".to_string()), "{after:?}");
    // The provider itself and every model under it are gone.
    assert!(!after.contains(&"anthropic".to_string()), "{after:?}");
    assert!(
        !after.contains(&"anthropic/claude-opus-4-6".to_string()),
        "{after:?}"
    );
    // Untouched providers remain listed.
    assert!(after.contains(&"openai".to_string()), "{after:?}");
}

#[tokio::test]
async fn disabled_pool_members_are_skipped_by_the_load_balancer() {
    let (server, settings, router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    disable_model(&server, &settings, id, "bad output").await;

    // Round-robin would otherwise hand out openai/gpt-5 on every other call.
    // With it disabled, the pool must only ever return the surviving member.
    let lb = LoadBalancer::new(settings.routing.load_balancer.clone());
    for _ in 0..6 {
        let picked = lb
            .resolve_available("pool", |p, m| router.is_available(p, m))
            .expect("pool still has an enabled member");
        assert_eq!(
            picked,
            ("anthropic".to_string(), "claude-opus-4-6".to_string())
        );
    }
    // And requests through the pool keep succeeding.
    for _ in 0..6 {
        chat(&server, "pool").await.assert_status_ok();
    }

    // With every member disabled the pool reports a clear 4xx rather than
    // silently falling through to the default model.
    let other = seed_model(&db, "anthropic", "claude-opus-4-6").await;
    disable_model(&server, &settings, other, "also bad").await;
    let res = chat(&server, "pool").await;
    assert_eq!(res.status_code(), axum::http::StatusCode::FORBIDDEN);
    assert!(res.text().contains("pool"), "{}", res.text());
}

#[tokio::test]
async fn an_alias_pointing_at_a_disabled_model_is_rejected() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    server
        .put("/admin/api/aliases/deep")
        .add_header(hk, hv)
        .json(&json!({ "target": "openai/gpt-5" }))
        .await
        .assert_status_ok();
    chat(&server, "deep").await.assert_status_ok();

    disable_model(&server, &settings, id, "quality regression").await;

    let res = chat(&server, "deep").await;
    assert_eq!(res.status_code(), axum::http::StatusCode::FORBIDDEN);
    assert!(res.text().contains("quality regression"));
}

#[tokio::test]
async fn an_operator_disable_is_distinct_from_a_circuit_breaker_trip() {
    let (server, settings, router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;

    // A breaker trip is transient and provider-scoped: it is recorded in the
    // breaker, auto-recovers, and never touches operator availability.
    let breaker = modelrouter::router::circuit_breaker::CircuitBreaker::new(1, 0);
    breaker.record_failure("openai");
    assert!(breaker.is_open("openai"));
    assert!(
        router.is_available("openai", "gpt-5"),
        "a breaker trip must not mark anything operator-disabled"
    );
    // Cooldown of 0 means the very next check half-opens it — it recovers on its own.
    assert!(!breaker.is_open("openai"));

    // An operator disable is sticky: no amount of success or elapsed time clears it.
    disable_model(&server, &settings, id, "manual takedown").await;
    breaker.record_success("openai");
    assert!(!breaker.is_open("openai"));
    assert!(
        !router.is_available("openai", "gpt-5"),
        "operator disable must survive breaker recovery"
    );
    assert_eq!(
        chat(&server, "openai/gpt-5").await.status_code(),
        axum::http::StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn disable_and_enable_are_audited_with_actor_and_reason() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    disable_model(&server, &settings, id, "cost spike").await;
    server
        .patch(&format!("/admin/api/models/{id}/enabled"))
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "enabled": true }))
        .await
        .assert_status_ok();
    server
        .patch("/admin/api/providers/openai/enabled")
        .add_header(hk, hv)
        .json(&json!({ "enabled": false, "reason": "vendor outage" }))
        .await
        .assert_status_ok();

    use modelrouter::db::repositories::audit::AuditRepository;
    let entries = AuditRepository::list(&*db, 50, 0).await.unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"model.disable"), "{actions:?}");
    assert!(actions.contains(&"model.enable"), "{actions:?}");
    assert!(actions.contains(&"provider.disable"), "{actions:?}");

    let disabled = entries.iter().find(|e| e.action == "model.disable").unwrap();
    assert_eq!(disabled.target.as_deref(), Some("model:openai/gpt-5"));
    assert!(disabled.actor_name.contains("superadmin"));
    assert!(disabled.after_json.as_ref().unwrap().contains("cost spike"));

    let provider = entries.iter().find(|e| e.action == "provider.disable").unwrap();
    assert_eq!(provider.target.as_deref(), Some("provider:openai"));
    assert!(provider.after_json.as_ref().unwrap().contains("vendor outage"));
}

#[tokio::test]
async fn disable_endpoints_require_admin_auth() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;

    // No token.
    server.get("/admin/api/models").await.assert_status_unauthorized();
    server.get("/admin/api/providers").await.assert_status_unauthorized();
    server
        .patch(&format!("/admin/api/models/{id}/enabled"))
        .json(&json!({ "enabled": false }))
        .await
        .assert_status_unauthorized();
    server
        .patch("/admin/api/providers/openai/enabled")
        .json(&json!({ "enabled": false }))
        .await
        .assert_status_unauthorized();

    // Viewers may read but not toggle.
    let (hk, hv) = bearer(&jwt(&settings, "viewer"));
    server
        .get("/admin/api/models")
        .add_header(hk.clone(), hv.clone())
        .await
        .assert_status_ok();
    server
        .patch(&format!("/admin/api/models/{id}/enabled"))
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "enabled": false }))
        .await
        .assert_status_forbidden();
    server
        .patch("/admin/api/providers/openai/enabled")
        .add_header(hk, hv)
        .json(&json!({ "enabled": false }))
        .await
        .assert_status_forbidden();

    // The rejected writes changed nothing.
    assert!(db.get_model(id).await.unwrap().unwrap().enabled);
}

/// The dashboard page is the operator's main surface for both features, so a
/// render regression (a bad template filter, a missing context key) must fail here.
#[tokio::test]
async fn models_dashboard_page_and_fragments_render() {
    let (server, settings, _router, db) = build_server().await;
    let id = seed_model(&db, "openai", "gpt-5").await;
    disable_model(&server, &settings, id, "cost spike").await;

    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let jwt_cookie = hv.to_str().unwrap().trim_start_matches("Bearer ").to_string();
    let cookie = (
        axum::http::header::COOKIE,
        axum::http::HeaderValue::from_str(&format!("mr_admin_session={jwt_cookie}")).unwrap(),
    );
    let _ = hk;

    let page = server
        .get("/admin/models")
        .add_header(cookie.0.clone(), cookie.1.clone())
        .await;
    page.assert_status_ok();
    let html = page.text();
    assert!(html.contains("Model Aliases"), "alias section missing");
    assert!(html.contains("Providers"), "provider section missing");
    assert!(html.contains("cost spike"), "disable reason not shown: {html}");
    assert!(html.contains("superadmin-user"), "disabling actor not shown");

    // htmx fragments used by those sections.
    let aliases = server
        .get("/admin/aliases/rows")
        .add_header(cookie.0.clone(), cookie.1.clone())
        .await;
    aliases.assert_status_ok();
    assert!(aliases.text().contains("No runtime aliases defined."));

    let providers = server
        .get("/admin/providers/rows")
        .add_header(cookie.0.clone(), cookie.1.clone())
        .await;
    providers.assert_status_ok();
    assert!(providers.text().contains("openai"));
    assert!(providers.text().contains("anthropic"));
}
