//! Admin dashboard budget management coverage.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::Settings;
use modelrouter::db::models::{NewAdminUser, NewBudgetRule, NewUser};
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::db::repositories::users::UserRepository;
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

fn cookie(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::COOKIE,
        axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
    )
}

#[tokio::test]
async fn get_budgets_page_requires_session() {
    let (server, _settings, _db) = build_server().await;
    let resp = server.get("/admin/budgets").await;
    assert_eq!(resp.status_code(), 303);
}

#[tokio::test]
async fn get_budgets_page_renders_cards() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "alice".to_string(), email: None })
        .await
        .unwrap();
    BudgetRepository::create(
        &*db,
        NewBudgetRule {
            user_id: Some(user.id),
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

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server.get("/admin/budgets").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("alice"));
    assert!(html.contains("$100.00"));
}

#[tokio::test]
async fn post_create_budget_requires_superadmin() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "monthly"),
            ("limit_usd", "50.0"),
        ])
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn post_create_budget_global_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "monthly"),
            ("limit_usd", "250.5"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("$250.50"));

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    let global = rules.iter().find(|r| r.window == "monthly" && r.user_id.is_none()).unwrap();
    assert_eq!(global.limit_usd, Some(250.5));
}

#[tokio::test]
async fn post_create_budget_requires_limit() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[("scope", "global"), ("window", "monthly")])
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("At least one of Limit USD or Limit Tokens is required"));
}

#[tokio::test]
async fn post_create_budget_rejects_duplicate_window() {
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
            limit_usd: Some(10.0),
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

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "monthly"),
            ("limit_usd", "50.0"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("monthly"));
    assert!(html.contains("already exists"));
}

#[tokio::test]
async fn post_create_budget_total_window_with_dates() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "total"),
            ("limit_usd", "1000.0"),
            ("window_start", "2026-01-01"),
            ("window_end", "2026-12-31"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    let total = rules.iter().find(|r| r.window == "total").unwrap();
    assert!(total.window_start.is_some());
    assert!(total.window_end.is_some());
}

#[tokio::test]
async fn post_create_budget_total_window_rejects_bad_dates() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "total"),
            ("limit_usd", "100.0"),
            ("window_start", "2026-12-31"),
            ("window_end", "2026-01-01"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("Start date must be before end date"));
}

#[tokio::test]
async fn post_create_budget_user_scope() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "bob".to_string(), email: None })
        .await
        .unwrap();
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "user"),
            ("user_id", &user.id.to_string()),
            ("window", "monthly"),
            ("limit_usd", "75.0"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    let user_rule = rules.iter().find(|r| r.user_id == Some(user.id)).unwrap();
    assert_eq!(user_rule.limit_usd, Some(75.0));
}

#[tokio::test]
async fn post_edit_budget_updates_limits() {
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

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/budgets/{}/edit", rule.id))
        .add_header(hk, hv)
        .form(&[("limit_usd", "200.0")])
        .await;
    assert_eq!(resp.status_code(), 200);

    let updated = BudgetRepository::list_all(&*db).await.unwrap();
    let rule = updated.iter().find(|r| r.id == rule.id).unwrap();
    assert_eq!(rule.limit_usd, Some(200.0));
}

#[tokio::test]
async fn post_delete_budget_removes_rule() {
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

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/budgets/{}/delete", rule.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);

    let remaining = BudgetRepository::list_all(&*db).await.unwrap();
    assert!(!remaining.iter().any(|r| r.id == rule.id));
}

#[tokio::test]
async fn post_create_budget_model_allow_deny() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "monthly"),
            ("limit_usd", "100.0"),
            ("model_allow", "claude-opus-4-5, gpt-4o"),
            ("model_deny", "gpt-3.5-turbo"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    let rule = rules.iter().find(|r| r.window == "monthly").unwrap();
    let allow: Vec<String> = serde_json::from_str(&rule.model_allow).unwrap();
    let deny: Vec<String> = serde_json::from_str(&rule.model_deny).unwrap();
    assert_eq!(allow, vec!["claude-opus-4-5", "gpt-4o"]);
    assert_eq!(deny, vec!["gpt-3.5-turbo"]);
}

#[tokio::test]
async fn post_create_budget_project_scope() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "project"),
            ("project", "demo"),
            ("window", "monthly"),
            ("limit_usd", "500.0"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    let proj = rules.iter().find(|r| r.project.as_deref() == Some("demo")).unwrap();
    assert_eq!(proj.limit_usd, Some(500.0));
}

#[tokio::test]
async fn post_create_budget_rate_rpm() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/budgets")
        .add_header(hk, hv)
        .form(&[
            ("scope", "global"),
            ("window", "monthly"),
            ("limit_usd", "100.0"),
            ("rate_rpm", "60"),
        ])
        .await;
    assert_eq!(resp.status_code(), 200);

    let rules = BudgetRepository::list_all(&*db).await.unwrap();
    let rule = rules.first().unwrap();
    assert_eq!(rule.rate_rpm, Some(60));
}
