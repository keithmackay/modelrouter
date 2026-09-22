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
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
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

/// Build a server whose `openai` provider points at `catalog_base`, so alias
/// writes validate against that live catalog (issues #35, #81).
async fn build_server_with_catalog(catalog_base: String) -> (TestServer, Arc<Settings>) {
    let db = common::in_memory_db().await;
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
    base.providers.insert(
        "openai".to_string(),
        modelrouter::config::schema::ProviderConfig {
            api_base: Some(catalog_base),
            api_key: "test-key".into(),
            ..Default::default()
        },
    );
    let settings = Arc::new(base);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));

    let state = AppState {
        settings: settings.clone(),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
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
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };

    let server = TestServer::new(build_router(state)).unwrap();
    (server, settings)
}

/// Issue #35: catalog validation when the catalog is available.
#[tokio::test]
async fn alias_target_validated_against_available_catalog() {
    use axum::routing::get;

    // Set up a mock catalog server
    let catalog_router = axum::Router::new().route(
        "/models",
        get(|| async {
            axum::Json(json!({
                "data": [
                    {"id": "gpt-4o"},
                    {"id": "gpt-4o-mini"}
                ]
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let catalog_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, catalog_router).await.unwrap();
    });
    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let (server, settings) =
        build_server_with_catalog(format!("http://{}", catalog_addr)).await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    // Model present in catalog → success (full provider/name format)
    server
        .put("/admin/api/aliases/fast")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "openai/gpt-4o" }))
        .await
        .assert_status_ok();

    // Model absent from an AVAILABLE catalog → rejected. This asserted OK
    // before issue #81: the process-global catalog cache let another test's
    // empty catalog satisfy this server's validation, and the pass depended
    // on that pollution. With the cache keyed by settings generation this
    // server sees its own (populated) catalog, so the write must 400.
    server
        .put("/admin/api/aliases/slow")
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "target": "missing-model" }))
        .await
        .assert_status_bad_request();
}

/// Issue #81: the catalog cache is process-global, so several servers in one
/// test binary share it. An entry cached for one server's settings must not
/// satisfy validation for a server with different settings — the flake was
/// alias writes 400ing against a catalog that belonged to a different test.
#[tokio::test]
async fn alias_validation_not_poisoned_by_other_servers_catalog() {
    use axum::routing::get;

    let catalog_router = axum::Router::new().route(
        "/models",
        get(|| async { axum::Json(json!({ "data": [ {"id": "gpt-4o"} ] })) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let catalog_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, catalog_router).await.unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Server A has a live catalog; a bogus target is rejected, which also
    // proves A's (populated) catalog is now in the shared cache.
    let (server_a, settings_a) =
        build_server_with_catalog(format!("http://{}", catalog_addr)).await;
    let (hk, hv) = bearer(&jwt(&settings_a, "superadmin"));
    server_a
        .put("/admin/api/aliases/poisoned")
        .add_header(hk, hv)
        .json(&json!({ "target": "openai/nope" }))
        .await
        .assert_status_bad_request();

    // Server B has NO catalog providers: its own catalog is unavailable, so
    // the same bogus target must be accepted (graceful degradation). Before
    // the fix B hit A's cached catalog and 400'd here.
    let (server_b, settings_b, _router, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings_b, "superadmin"));
    server_b
        .put("/admin/api/aliases/poisoned")
        .add_header(hk, hv)
        .json(&json!({ "target": "openai/nope" }))
        .await
        .assert_status_ok();
}

/// Issue #35: graceful degradation when catalog is unavailable.
#[tokio::test]
async fn alias_write_degrades_when_catalog_unavailable() {
    // Build server with no catalog provider configured (catalog will be empty/unavailable)
    let (server, settings, _router, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));

    // Should succeed even though we can't validate against the catalog
    server
        .put("/admin/api/aliases/deep")
        .add_header(hk, hv)
        .json(&json!({ "target": "anthropic/claude-opus-4-6" }))
        .await
        .assert_status_ok();
}

/// Catalog listing degradation: one working provider + one failing provider.
/// The available-models endpoint should return the working provider's models
/// rather than failing the entire request (per 09-05 review).
#[tokio::test]
async fn catalog_listing_degrades_with_one_provider_failing() {
    use axum::routing::get;

    // Set up two mock catalog servers: one working, one failing
    let working_router = axum::Router::new().route(
        "/models",
        get(|| async {
            axum::Json(json!({
                "data": [
                    {"id": "gpt-4o"},
                    {"id": "gpt-4o-mini"}
                ]
            }))
        }),
    );
    let working_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let working_addr = working_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(working_listener, working_router).await.unwrap();
    });
    let working_base = format!("http://{}", working_addr);

    let failing_router = axum::Router::new().route(
        "/models",
        get(|| async {
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "provider down")
        }),
    );
    let failing_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failing_addr = failing_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(failing_listener, failing_router).await.unwrap();
    });
    let failing_base = format!("http://{}", failing_addr);

    // Give the servers a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Aggregate directly rather than through /admin/api/models/available: the
    // endpoint serves this aggregation from a process-wide TTL cache that
    // sibling tests in this binary also populate, so endpoint-level assertions
    // would race them. The degradation contract lives in the aggregation.
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
        modelrouter::config::schema::ProviderConfig {
            api_base: Some(working_base),
            api_key: "test-key".into(),
            ..Default::default()
        },
    );
    providers.insert(
        "groq".to_string(),
        modelrouter::config::schema::ProviderConfig {
            api_base: Some(failing_base),
            api_key: "test-key".into(),
            ..Default::default()
        },
    );

    let out = modelrouter::providers::catalog_registry::aggregate_catalogs(&providers).await;

    // The working provider's models are present
    assert_eq!(out["openai"]["supported"], true);
    let models = out["openai"]["models"].as_array().unwrap();
    assert!(!models.is_empty(), "working provider should have models");
    assert_eq!(models[0]["name"], "gpt-4o");

    // The failing provider degrades to an error entry; the aggregate as a
    // whole still succeeds rather than failing the request.
    assert_eq!(out["groq"]["supported"], true);
    assert!(
        out["groq"]["error"].as_str().unwrap().contains("500"),
        "failing provider should carry an error"
    );
    assert!(out["groq"].get("models").is_none(), "failing provider should have no models");
}

// ── Dashboard (htmx form) surface ────────────────────────────────────────────
//
// The JSON API above and the dashboard form handlers are separate code paths
// over the same validation: the form answers with an HTML fragment and a
// session cookie instead of a JSON body and a bearer token, so an error that
// only bites the form (a 500 where an inline alert belongs, a row fragment
// that never renders) is invisible to the API tests.

fn session(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::COOKIE,
        axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
    )
}

#[tokio::test]
async fn dashboard_form_creates_then_updates_an_alias_and_renders_rows() {
    let (server, settings, router, db) = build_server().await;
    let (ck, cv) = session(&jwt(&settings, "superadmin"));

    // Empty table first: the fragment says so rather than rendering nothing.
    let empty = server
        .get("/admin/aliases/rows")
        .add_header(ck.clone(), cv.clone())
        .await;
    empty.assert_status_ok();
    assert!(empty.text().contains("No runtime aliases defined"), "{}", empty.text());

    // Create.
    let created = server
        .post("/admin/aliases")
        .add_header(ck.clone(), cv.clone())
        .form(&[("alias", "balanced"), ("target", "anthropic/claude-sonnet-4-5")])
        .await;
    created.assert_status_ok();
    let body = created.text();
    assert!(body.contains("saved and live"), "{body}");
    assert!(body.contains("balanced"), "{body}");
    assert_eq!(router.resolve("balanced").1, "claude-sonnet-4-5");

    // The row fragment now renders the alias, its target and a delete control.
    let rows = server
        .get("/admin/aliases/rows")
        .add_header(ck.clone(), cv.clone())
        .await;
    let rows_html = rows.text();
    assert!(rows_html.contains("alias-row-balanced"), "{rows_html}");
    assert!(rows_html.contains("anthropic/claude-sonnet-4-5"), "{rows_html}");
    assert!(rows_html.contains("/admin/aliases/balanced/delete"), "{rows_html}");
    assert!(rows_html.contains("superadmin-user"), "created_by is shown: {rows_html}");

    // Update the same alias — upsert, not a duplicate row, and audited as an update.
    server
        .post("/admin/aliases")
        .add_header(ck.clone(), cv.clone())
        .form(&[("alias", "balanced"), ("target", "openai/gpt-5-mini")])
        .await
        .assert_status_ok();
    let rows_html = server
        .get("/admin/aliases/rows")
        .add_header(ck.clone(), cv.clone())
        .await
        .text();
    assert_eq!(
        rows_html.matches("<tr id=\"alias-row-balanced\"").count(),
        1,
        "upsert replaces the row, it does not add one: {rows_html}"
    );
    assert!(rows_html.contains("openai/gpt-5-mini"), "{rows_html}");

    use modelrouter::db::repositories::audit::AuditRepository;
    let entries = AuditRepository::list(&*db, 50, 0).await.unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"alias.create"), "{actions:?}");
    assert!(actions.contains(&"alias.update"), "{actions:?}");
}

#[tokio::test]
async fn dashboard_form_reports_validation_failures_inline() {
    let (server, settings, _router, _db) = build_server().await;
    let (ck, cv) = session(&jwt(&settings, "superadmin"));

    // Missing target: an inline alert, not a 4xx the htmx swap would discard.
    let blank = server
        .post("/admin/aliases")
        .add_header(ck.clone(), cv.clone())
        .form(&[("alias", "balanced"), ("target", "   ")])
        .await;
    blank.assert_status_ok();
    assert!(blank.text().contains("alert-danger"), "{}", blank.text());
    assert!(blank.text().contains("required"), "{}", blank.text());

    // Reserved ':' prefix.
    let reserved = server
        .post("/admin/aliases")
        .add_header(ck.clone(), cv.clone())
        .form(&[("alias", ":fast"), ("target", "openai/gpt-4o")])
        .await;
    reserved.assert_status_ok();
    assert!(reserved.text().contains("alert-danger"), "{}", reserved.text());

    // Self-reference, and the message is HTML-escaped on the way back out.
    let cycle = server
        .post("/admin/aliases")
        .add_header(ck.clone(), cv.clone())
        .form(&[("alias", "<b>loop</b>"), ("target", "<b>loop</b>")])
        .await;
    cycle.assert_status_ok();
    let text = cycle.text();
    assert!(text.contains("alert-danger"), "{text}");
    assert!(!text.contains("<b>loop</b>"), "message must be escaped: {text}");
}

#[tokio::test]
async fn dashboard_form_deletes_an_alias() {
    let (server, settings, router, _db) = build_server().await;
    let (ck, cv) = session(&jwt(&settings, "superadmin"));

    server
        .post("/admin/aliases")
        .add_header(ck.clone(), cv.clone())
        .form(&[("alias", "balanced"), ("target", "openai/gpt-4o")])
        .await
        .assert_status_ok();
    assert_eq!(router.resolve("balanced").1, "gpt-4o");

    // htmx swaps the row out, so the delete handler answers with an empty body.
    let deleted = server
        .post("/admin/aliases/balanced/delete")
        .add_header(ck.clone(), cv.clone())
        .await;
    deleted.assert_status_ok();
    assert!(deleted.text().is_empty(), "{:?}", deleted.text());

    let rows = server
        .get("/admin/aliases/rows")
        .add_header(ck.clone(), cv.clone())
        .await;
    assert!(rows.text().contains("No runtime aliases defined"), "{}", rows.text());

    // Deleting an alias that is already gone is idempotent on this surface —
    // the row the swap targets is removed either way.
    server
        .post("/admin/aliases/balanced/delete")
        .add_header(ck, cv)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn dashboard_alias_writes_require_a_superadmin_session() {
    let (server, settings, _router, _db) = build_server().await;
    let (ck, cv) = session(&jwt(&settings, "viewer"));

    for resp in [
        server
            .post("/admin/aliases")
            .add_header(ck.clone(), cv.clone())
            .form(&[("alias", "balanced"), ("target", "openai/gpt-4o")])
            .await,
        server
            .post("/admin/aliases/balanced/delete")
            .add_header(ck.clone(), cv.clone())
            .await,
        server.get("/admin/aliases/rows").add_header(ck, cv).await,
    ] {
        assert!(
            !resp.status_code().is_success(),
            "a viewer session must not reach alias writes, got {}",
            resp.status_code()
        );
    }
}
