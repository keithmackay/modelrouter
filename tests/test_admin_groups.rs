//! Admin dashboard group management coverage.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::Settings;
use modelrouter::db::models::{NewAdminUser, NewUser};
use modelrouter::db::repositories::admin_users::AdminUserRepository;
use modelrouter::db::repositories::groups::GroupRepository;
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

// ── Get groups ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_groups_requires_session() {
    let (server, _settings, _db) = build_server().await;
    let resp = server.get("/admin/groups").await;
    assert_eq!(resp.status_code(), 303);
}

#[tokio::test]
async fn get_groups_renders_page() {
    let (server, settings, db) = build_server().await;
    GroupRepository::create_group(&*db, "team-a", 10).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server.get("/admin/groups").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("team-a"));
    assert!(html.contains("priority 10"));
}

#[tokio::test]
async fn get_groups_shows_memberships() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-b", 0).await.unwrap();
    let user = UserRepository::create(&*db, NewUser { name: "alice".to_string(), email: None })
        .await
        .unwrap();
    GroupRepository::add_member(&*db, group.id, user.id).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server.get("/admin/groups").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("alice"));
}

// ── Create group ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_create_group_requires_superadmin() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server
        .post("/admin/groups")
        .add_header(hk, hv)
        .form(&[("name", "new-group")])
        .await;
    assert_eq!(resp.status_code(), 403);
}

#[tokio::test]
async fn post_create_group_success() {
    let (server, settings, db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/groups")
        .add_header(hk, hv)
        .form(&[("name", "team-c"), ("priority", "5")])
        .await;
    assert_eq!(resp.status_code(), 200);

    let groups = GroupRepository::list_groups(&*db).await.unwrap();
    let created = groups.iter().find(|g| g.name == "team-c").unwrap();
    assert_eq!(created.priority, 5);
}

#[tokio::test]
async fn post_create_group_empty_name() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/groups")
        .add_header(hk, hv)
        .form(&[("name", "  ")])
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn post_create_group_duplicate_name() {
    let (server, settings, db) = build_server().await;
    GroupRepository::create_group(&*db, "duplicate", 0).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/groups")
        .add_header(hk, hv)
        .form(&[("name", "duplicate")])
        .await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("duplicate"));
    assert!(html.contains("already exists"));
}

// ── Enable/disable group ──────────────────────────────────────────────────────

#[tokio::test]
async fn post_disable_group_success() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-d", 0).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/disable", group.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);

    let updated = GroupRepository::get_group(&*db, group.id).await.unwrap().unwrap();
    assert!(!updated.enabled);
}

#[tokio::test]
async fn post_enable_group_success() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-e", 0).await.unwrap();
    GroupRepository::set_group_enabled(&*db, group.id, false).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/enable", group.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);

    let updated = GroupRepository::get_group(&*db, group.id).await.unwrap().unwrap();
    assert!(updated.enabled);
}

// ── Set priority ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_set_group_priority_success() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-f", 0).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/priority", group.id))
        .add_header(hk, hv)
        .form(&[("priority", "20")])
        .await;
    assert_eq!(resp.status_code(), 200);

    let updated = GroupRepository::get_group(&*db, group.id).await.unwrap().unwrap();
    assert_eq!(updated.priority, 20);
}

// ── Add member ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_add_member_success() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-g", 0).await.unwrap();
    let user = UserRepository::create(&*db, NewUser { name: "bob".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/members", group.id))
        .add_header(hk, hv)
        .form(&[("user_id", &user.id.to_string())])
        .await;
    assert_eq!(resp.status_code(), 200);

    let memberships = GroupRepository::list_memberships(&*db, group.id).await.unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].user_id, user.id);
}

#[tokio::test]
async fn post_add_member_to_disabled_group() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-h", 0).await.unwrap();
    GroupRepository::set_group_enabled(&*db, group.id, false).await.unwrap();
    let user = UserRepository::create(&*db, NewUser { name: "charlie".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/members", group.id))
        .add_header(hk, hv)
        .form(&[("user_id", &user.id.to_string())])
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn post_add_member_duplicate() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-i", 0).await.unwrap();
    let user = UserRepository::create(&*db, NewUser { name: "dave".to_string(), email: None })
        .await
        .unwrap();
    GroupRepository::add_member(&*db, group.id, user.id).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/members", group.id))
        .add_header(hk, hv)
        .form(&[("user_id", &user.id.to_string())])
        .await;
    assert_eq!(resp.status_code(), 400);
}

// ── Disable member ────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_disable_member_success() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-j", 0).await.unwrap();
    let user = UserRepository::create(&*db, NewUser { name: "eve".to_string(), email: None })
        .await
        .unwrap();
    GroupRepository::add_member(&*db, group.id, user.id).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/members/{}/disable", group.id, user.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 200);

    let memberships = GroupRepository::list_memberships(&*db, group.id).await.unwrap();
    assert!(memberships[0].disabled_at.is_some());
}

#[tokio::test]
async fn post_disable_member_not_found() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-k", 0).await.unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post(&format!("/admin/groups/{}/members/9999/disable", group.id))
        .add_header(hk, hv)
        .await;
    assert_eq!(resp.status_code(), 404);
}

// ── Group not found ───────────────────────────────────────────────────────────

#[tokio::test]
async fn post_enable_group_not_found() {
    let (server, settings, _db) = build_server().await;
    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server.post("/admin/groups/9999/enable").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn post_add_member_group_not_found() {
    let (server, settings, db) = build_server().await;
    let user = UserRepository::create(&*db, NewUser { name: "frank".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "superadmin"));
    let resp = server
        .post("/admin/groups/9999/members")
        .add_header(hk, hv)
        .form(&[("user_id", &user.id.to_string())])
        .await;
    assert_eq!(resp.status_code(), 404);
}

// ── Card rendering with all users ─────────────────────────────────────────────

#[tokio::test]
async fn get_groups_shows_add_member_select() {
    let (server, settings, db) = build_server().await;
    let group = GroupRepository::create_group(&*db, "team-l", 0).await.unwrap();
    UserRepository::create(&*db, NewUser { name: "grace".to_string(), email: None })
        .await
        .unwrap();
    UserRepository::create(&*db, NewUser { name: "henry".to_string(), email: None })
        .await
        .unwrap();

    let (hk, hv) = cookie(&jwt(&settings, "admin"));
    let resp = server.get("/admin/groups").add_header(hk, hv).await;
    assert_eq!(resp.status_code(), 200);
    let html = resp.text();
    assert!(html.contains("grace"));
    assert!(html.contains("henry"));
    assert!(html.contains(&format!("/admin/groups/{}/members", group.id)));
}
