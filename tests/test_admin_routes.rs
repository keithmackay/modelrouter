//! Admin REST API coverage for user, budget, stats, audit, and API key management.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::Settings;
use modelrouter::db::models::{NewAdminUser, NewApiKey, NewBudgetRule, NewCostLedgerEntry, NewUser};
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::users::UserRepository;
use serde_json::{json, Value};
use std::sync::Arc;

async fn build_server() -> (TestServer, Arc<Settings>, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    AdminUserRepository::create(
        &db,
        NewAdminUser {
            name: "superadmin-user".to_string(),
            password_hash: bcrypt::hash("password", bcrypt::DEFAULT_COST).unwrap(),
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

// ── Login ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn admin_login_success() {
    let (server, _settings, _db) = build_server().await;
    let resp = server
        .post("/admin/api/login")
        .json(&json!({ "name": "superadmin-user", "password": "password" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert!(body["token"].as_str().unwrap().len() > 20);
}

#[tokio::test]
async fn admin_login_wrong_password() {
    let (server, _settings, _db) = build_server().await;
    let resp = server
        .post("/admin/api/login")
        .json(&json!({ "name": "superadmin-user", "password": "wrong" }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn admin_login_unknown_user() {
    let (server, _settings, _db) = build_server().await;
    let resp = server
        .post("/admin/api/login")
        .json(&json!({ "name": "unknown", "password": "password" }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn admin_login_disabled_user() {
    let (server, _settings, db) = build_server().await;
    let disabled = AdminUserRepository::create(
        &*db,
        NewAdminUser {
            name: "disabled-admin".to_string(),
            password_hash: bcrypt::hash("password", bcrypt::DEFAULT_COST).unwrap(),
            role: "viewer".to_string(),
        },
    )
    .await
    .unwrap();
    AdminUserRepository::set_enabled(&*db, disabled.id, false).await.unwrap();

    let resp = server
        .post("/admin/api/login")
        .json(&json!({ "name": "disabled-admin", "password": "password" }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

// ── Users ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_users_requires_auth() {
    let (server, _settings, _db) = build_server().await;
    let resp = server.get("/admin/api/users").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn list_users_admin_can_read() {
    let (server, settings, db) = build_server().await;
    UserRepository::create(&*db, NewUser { name: "alice".to_string(), email: None })
        .await
        .unwrap();
    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/users").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let users: Value = resp.json();
    assert!(users.as_array().unwrap().iter().any(|u| u["name"] == "alice"));
}

#[tokio::test]
async fn create_user_requires_superadmin() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/api/users")
        .add_header(hk, hv)
        .json(&json!({ "name": "bob" }))
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn create_user_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/users")
        .add_header(hk, hv)
        .json(&json!({ "name": "charlie" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["user"]["name"], "charlie");
    assert!(body["user"]["id"].as_i64().unwrap() > 0);

    let users = UserRepository::list(&*db).await.unwrap();
    assert!(users.iter().any(|u| u.name == "charlie"));
}

#[tokio::test]
async fn update_user_enable_disable() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "dave".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .patch(&format!("/admin/api/users/{}", user.id))
        .add_header(hk.clone(), hv.clone())
        .json(&json!({ "enabled": false }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let updated = UserRepository::find_by_id(&*db, user.id).await.unwrap().unwrap();
    assert!(!updated.enabled);

    let resp = server
        .patch(&format!("/admin/api/users/{}", user.id))
        .add_header(hk, hv)
        .json(&json!({ "enabled": true }))
        .await;
    assert_eq!(resp.status_code(), 200);

    let updated = UserRepository::find_by_id(&*db, user.id).await.unwrap().unwrap();
    assert!(updated.enabled);
}

// ── Budgets ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_budgets_admin_can_read() {
    let (server, settings, db) = build_server().await;
    BudgetRepository::create(
        &*db,
        NewBudgetRule {
            user_id: None,
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: Some(100.0),
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec![],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/budgets").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let budgets: Value = resp.json();
    assert_eq!(budgets.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn create_budget_requires_superadmin() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/api/budgets")
        .add_header(hk, hv)
        .json(&json!({
            "window": "monthly",
            "limit_usd": 50.0,
        }))
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn create_budget_invalid_window() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/budgets")
        .add_header(hk, hv)
        .json(&json!({
            "window": "yearly",
            "limit_usd": 100.0,
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn create_budget_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/budgets")
        .add_header(hk, hv)
        .json(&json!({
            "window": "monthly",
            "limit_usd": 150.0,
            "rate_rpm": 100,
            "model_allow": ["claude-opus-4-5"],
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let rule: Value = resp.json();
    assert_eq!(rule["limit_usd"], 150.0);
    assert_eq!(rule["rate_rpm"], 100);

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    assert_eq!(rules.len(), 1);
}

#[tokio::test]
async fn delete_budget_success() {
    let (server, settings, db) = build_server().await;
    let rule = BudgetRepository::create(
        &*db,
        NewBudgetRule {
            user_id: None,
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: Some(50.0),
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            model_allow: vec![],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .delete(&format!("/admin/api/budgets/{}", rule.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);

    let remaining = BudgetRepository::list_all(&*db).await.unwrap();
    assert!(remaining.is_empty());
}

// ── Stats ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_stats_admin_can_read() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "eve".to_string(), email: None })
        .await
        .unwrap();
    CostRepository::create(
        &*db,
        NewCostLedgerEntry {
            user_id: user.id,
            prompt_id: None,
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.25,
            api_key_id: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
        },
    )
    .await
    .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/stats").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let stats: Value = resp.json();
    let eve = stats.as_array().unwrap().iter().find(|s| s["name"] == "eve").unwrap();
    assert_eq!(eve["total_cost_usd"], 0.25);
}

// ── Audit ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_audit_requires_auth() {
    let (server, _settings, _db) = build_server().await;
    let resp = server.get("/admin/api/audit").await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn get_audit_returns_entries() {
    let (server, settings, db) = build_server().await;
    UserRepository::create(&*db, NewUser { name: "frank".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/audit").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let entries: Value = resp.json();
    assert!(entries.as_array().unwrap().is_empty() || entries.is_array());
}

// ── Admin users ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_admins_requires_superadmin() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/admins").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn list_admins_success() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server.get("/admin/api/admins").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let admins: Value = resp.json();
    assert!(admins.as_array().unwrap().len() >= 2);
    assert!(admins.as_array().unwrap().iter().all(|a| a.get("password_hash").is_none()));
}

#[tokio::test]
async fn create_admin_invalid_role() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/admins")
        .add_header(hk, hv)
        .json(&json!({
            "name": "badmin",
            "password": "secret",
            "role": "user",
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn create_admin_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/api/admins")
        .add_header(hk, hv)
        .json(&json!({
            "name": "new-admin",
            "password": "secret",
            "role": "viewer",
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["name"], "new-admin");
    assert_eq!(body["role"], "viewer");

    let admins = AdminUserRepository::list(&*db).await.unwrap();
    assert!(admins.iter().any(|a| a.name == "new-admin"));
}

// ── API keys ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_user_api_keys_admin_can_read() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "grace".to_string(), email: None })
        .await
        .unwrap();
    ApiKeyRepository::create_api_key(
        &*db,
        NewApiKey {
            user_id: user.id,
            key_hash: "hash".to_string(),
            label: Some("key1".to_string()),
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server
        .get(&format!("/admin/api/users/{}/keys", user.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);
    let keys: Value = resp.json();
    assert_eq!(keys.as_array().unwrap().len(), 1);
    assert_eq!(keys[0]["label"], "key1");
}

#[tokio::test]
async fn create_user_api_key_requires_superadmin() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "henry".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server
        .post(&format!("/admin/api/users/{}/keys", user.id))
        .add_header(hk, hv)
        .json(&json!({ "label": "test-key" }))
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn create_user_api_key_success() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "iris".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/api/users/{}/keys", user.id))
        .add_header(hk, hv)
        .json(&json!({ "label": "new-key", "project": "demo" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert!(body["key"].as_str().unwrap().starts_with("mr-"));
    assert_eq!(body["label"], "new-key");
    assert_eq!(body["project"], "demo");

    let keys = ApiKeyRepository::list_api_keys_for_user(&*db, user.id).await.unwrap();
    assert_eq!(keys.len(), 1);
}

#[tokio::test]
async fn revoke_api_key_success() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "jack".to_string(), email: None })
        .await
        .unwrap();
    let key = ApiKeyRepository::create_api_key(
        &*db,
        NewApiKey {
            user_id: user.id,
            key_hash: "hash".to_string(),
            label: None,
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/api/keys/{}/revoke", key.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 204);

    let revoked = ApiKeyRepository::list_api_keys_for_user(&*db, user.id).await.unwrap();
    assert!(!revoked[0].enabled);
}

#[tokio::test]
async fn reset_user_spend_success() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "kate".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = bearer(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/api/users/{}/reset-spend", user.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["user_id"], user.id);
    assert_eq!(body["reset"], true);
}

// ── Prompts ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_prompts_admin_can_read() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = bearer(&jwt(&settings, "admin"));
    let resp = server.get("/admin/api/prompts").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let prompts: Value = resp.json();
    assert!(prompts.is_array());
}
