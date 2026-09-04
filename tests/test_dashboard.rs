mod common;

use axum_test::TestServer;
use modelrouter::api::app::{AppState, DatabaseProvider, build_router};
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    cost::CostCalculator,
    engine::RequestRouter,
    fallback::FallbackChain,
    policy::PolicyEngine,
};
use modelrouter::config::Settings;
use std::collections::HashMap;
use std::sync::Arc;

async fn build_test_server() -> TestServer {
    let db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    build_test_server_with_db(Arc::new(db), settings).await
}

/// Same wiring as `build_test_server`, but over a caller-supplied database so a
/// test can seed rows before the server reads them.
async fn build_test_server_with_db(
    db: Arc<modelrouter::db::sqlite::SqliteDb>,
    settings: Arc<Settings>,
) -> TestServer {
    let db: Arc<dyn DatabaseProvider> = db;
    let registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry: registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };

    TestServer::new(build_router(state)).unwrap()
}

fn viewer_jwt(settings: &Settings) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    let claims = AdminClaims {
        sub: 999,
        name: "viewer-user".to_string(),
        role: "viewer".to_string(),
        exp,
    };
    issue_jwt(&claims, &settings.auth.jwt_secret).unwrap()
}

// Test 1: Unauthenticated GET /admin → 303 redirect to /admin/login
#[tokio::test]
async fn unauthenticated_redirect() {
    let server = build_test_server().await;
    let resp = server.get("/admin").await;
    assert_eq!(resp.status_code(), 303, "GET /admin without cookie should redirect");
    let location = resp.headers().get("location").expect("should have location header");
    assert_eq!(location.to_str().unwrap(), "/admin/login");
}

// Test 2: GET /admin/login → 200 with HTML form
#[tokio::test]
async fn login_renders_form() {
    let server = build_test_server().await;
    let resp = server.get("/admin/login").await;
    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("<form"), "login page should contain a form");
    assert!(body.contains("password"), "login page should have password field");
}

// Test 3: POST /admin/login with valid credentials → 303 + Set-Cookie
#[tokio::test]
async fn login_success_sets_cookie() {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    // Create an admin user in DB (cost=4 for speed in tests)
    let password = "test-password-123";
    let password_hash = bcrypt::hash(password, 4).unwrap();
    AdminUserRepository::create(
        &raw_db,
        NewAdminUser {
            name: "testadmin".to_string(),
            password_hash,
            role: "superadmin".to_string(),
        },
    )
    .await
    .unwrap();

    let db: Arc<dyn DatabaseProvider> = Arc::new(raw_db);
    let registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry: registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };

    let server = TestServer::new(build_router(state)).unwrap();

    let resp = server
        .post("/admin/login")
        .form(&[("username", "testadmin"), ("password", password)])
        .await;

    assert_eq!(resp.status_code(), 303, "successful login should redirect (got {})", resp.status_code());
    let set_cookie = resp.headers().get("set-cookie").expect("should set a cookie");
    let cookie_str = set_cookie.to_str().unwrap();
    assert!(cookie_str.contains("mr_admin_session="), "should set mr_admin_session cookie");
    assert!(cookie_str.to_lowercase().contains("httponly"), "cookie should be HttpOnly");
}

// Test 4: GET /admin/admins with a viewer JWT cookie → 403
#[tokio::test]
async fn superadmin_only_admins_page() {
    let db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "ok".to_string(),
    }));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let policy = Arc::new(PolicyEngine::new(db.clone()));

    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        )
    );

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry: registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };

    let server = TestServer::new(build_router(state)).unwrap();

    let resp = server
        .get("/admin/admins")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 403, "viewer role should get 403 on /admin/admins");
}

/// The Failures page must render the captured failures, grouped by stage.
///
/// Capturing failures in the database is only half the job: until this page
/// existed the dashboard could answer "what ran" but not "what failed", and an
/// operator's only recourse was the calling application's own logs — which is
/// exactly the dead end that left 196 "Unknown provider" errors undiagnosed for a
/// full run.
#[tokio::test]
async fn failures_page_lists_captured_failures_by_stage() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "anthropic/claude-sonnet-4".to_string(),
            routed_model: Some("claude-sonnet-4".to_string()),
            provider: Some("anthropic".to_string()),
            stage: FailureStage::Resolve,
            status_code: Some(502),
            error_message: "provider error: Unknown provider: anthropic".to_string(),
            attempts: 1,
            latency_ms: Some(3),
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
        },
    )
    .await
    .expect("failure should persist");

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "failures page should render for an admin");
    let body = resp.text();
    // The template HTML-escapes, so the slash arrives as &#x2f; — assert on the
    // distinctive part of the name rather than the raw literal.
    assert!(
        body.contains("claude-sonnet-4"),
        "the failing model must be shown: {body}"
    );
    assert!(
        body.contains("Unknown provider: anthropic"),
        "the provider's own message must be shown verbatim"
    );
    assert!(
        body.contains("resolve"),
        "the stage must be shown so the operator knows it is a config fault"
    );
}

// ── Compare page ──────────────────────────────────────────────────────────────

fn session_cookie(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::COOKIE,
        axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
    )
}

/// Ledger rows for one arm. The ledger references `users`, so the first call
/// creates the row every entry points at.
async fn seed_compare_ledger(
    db: &modelrouter::db::sqlite::SqliteDb,
    model: &str,
    provider: &str,
    tags: &str,
    n: usize,
) {
    use modelrouter::db::models::{NewCostLedgerEntry, NewUser};
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::users::UserRepository;

    if UserRepository::find_by_name(db, "compare-user").await.unwrap().is_none() {
        UserRepository::create(db, NewUser { name: "compare-user".to_string(), email: None })
            .await
            .unwrap();
    }
    let user = UserRepository::find_by_name(db, "compare-user").await.unwrap().unwrap();
    for _ in 0..n {
        CostRepository::create(
            db,
            NewCostLedgerEntry {
                user_id: user.id,
                prompt_id: None,
                model: model.to_string(),
                provider: provider.to_string(),
                project: None,
                tokens_in: 10,
                tokens_out: 5,
                cost_usd: 0.01,
                api_key_id: None,
                attribution_correlation_id: Some(format!("run-{model}")),
                attribution_tags: tags.to_string(),
                experiment_id: None,
                experiment_variant: None,
                tokens_estimated: false,
            },
        )
        .await
        .unwrap();
    }
}

/// Reverse minijinja's HTML escaping so a `data-chart-data` attribute can be
/// parsed back as JSON.
fn html_unescape(s: &str) -> String {
    s.replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#x2f;", "/")
        .replace("&#47;", "/")
        .replace("&amp;", "&")
}

/// Every `data-chart-data="..."` attribute value in `body`, keyed by the `id`
/// that precedes it on the same element.
fn chart_data_attrs(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let marker = "data-chart-data=\"";
    let mut from = 0;
    while let Some(pos) = body[from..].find(marker) {
        let start = from + pos + marker.len();
        let value = body[start..].split('"').next().unwrap_or("");
        let id = body[..from + pos]
            .rsplit("id=\"")
            .next()
            .and_then(|s| s.split('"').next())
            .unwrap_or("")
            .to_string();
        out.insert(id, value.to_string());
        from = start;
    }
    out
}

#[tokio::test]
async fn compare_page_renders_for_viewer() {
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(common::in_memory_db().await), settings).await;

    let resp = server
        .get("/admin/compare")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(resp.status_code(), 200, "viewer should see the compare page");
    let body = resp.text();
    assert!(body.contains("name=\"dimension\""), "dimension selector missing: {body}");
    assert!(body.contains("href=\"/admin/compare\""), "nav link missing");
    assert!(
        body.contains("no quality column"),
        "the quality caveat must be visible on the page itself"
    );
}

#[tokio::test]
async fn compare_pickers_and_panels_list_both_models() {
    let raw_db = common::in_memory_db().await;
    seed_compare_ledger(&raw_db, "mock-model", "mock", "{}", 3).await;
    seed_compare_ledger(&raw_db, "mock-model-b", "mock", "{}", 2).await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let page = server
        .get("/admin/compare")
        .add_query_param("dimension", "model")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(page.status_code(), 200);
    let body = page.text();
    assert!(body.contains("value=\"mock-model\""), "picker must list mock-model: {body}");
    assert!(body.contains("value=\"mock-model-b\""), "picker must list mock-model-b");

    let panels = server
        .get("/admin/compare/panels")
        .add_query_param("dimension", "model")
        .add_query_param("a", "mock-model")
        .add_query_param("b", "mock-model-b")
        .add_query_param("window", "all")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(panels.status_code(), 200, "{}", panels.text());
    let body = panels.text();
    assert!(body.contains("mock-model"), "arm A label missing: {body}");
    assert!(body.contains("mock-model-b"), "arm B label missing");
    assert!(body.contains(">3<"), "arm A request count missing: {body}");
    assert!(body.contains(">2<"), "arm B request count missing: {body}");
}

#[tokio::test]
async fn compare_panels_tag_dimension_with_no_rows_says_no_data() {
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(common::in_memory_db().await), settings).await;

    let resp = server
        .get("/admin/compare/panels")
        .add_query_param("dimension", "tag")
        .add_query_param("key", "arm")
        .add_query_param("a", "control")
        .add_query_param("b", "treatment")
        .add_query_param("window", "all")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(resp.status_code(), 200, "an empty arm is not an error: {}", resp.text());
    let body = resp.text();
    assert!(body.to_lowercase().contains("no data"), "empty arms must say so: {body}");
}

#[tokio::test]
async fn compare_panels_unsafe_tag_key_renders_inline_message() {
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(common::in_memory_db().await), settings).await;

    let resp = server
        .get("/admin/compare/panels")
        .add_query_param("dimension", "tag")
        .add_query_param("key", "a b")
        .add_query_param("a", "x")
        .add_query_param("b", "y")
        .add_query_param("window", "all")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(resp.status_code(), 200, "validation is shown inline, not as an error page");
    let body = resp.text();
    assert!(body.contains("tag key"), "validation message must name the field: {body}");
    assert!(!body.contains("<h1>Bad Request</h1>"), "must not be the generic error page");
}

#[tokio::test]
async fn compare_panels_carry_chart_data_json() {
    let raw_db = common::in_memory_db().await;
    seed_compare_ledger(&raw_db, "mock-model", "mock", "{}", 2).await;
    seed_compare_ledger(&raw_db, "mock-model-b", "mock", "{}", 1).await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/compare/panels")
        .add_query_param("dimension", "model")
        .add_query_param("a", "mock-model")
        .add_query_param("b", "mock-model-b")
        .add_query_param("window", "all")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    let charts = chart_data_attrs(&body);
    for id in ["compare-bars-chart", "compare-daily-chart", "compare-latency-chart"] {
        let raw = charts
            .get(id)
            .unwrap_or_else(|| panic!("{id} must carry data-chart-data: {body}"));
        let parsed: serde_json::Value = serde_json::from_str(&html_unescape(raw))
            .unwrap_or_else(|e| panic!("{id} chart data is not JSON ({e}): {raw}"));
        assert!(parsed.is_object() || parsed.is_array(), "{id} chart data must be structured");
    }
    let bars: serde_json::Value =
        serde_json::from_str(&html_unescape(&charts["compare-bars-chart"])).unwrap();
    assert_eq!(bars["a"]["label"], "mock-model");
    assert_eq!(bars["b"]["label"], "mock-model-b");
}

#[tokio::test]
async fn compare_panels_auth_matches_reports_panels() {
    let settings = Arc::new(Settings::default());
    let server =
        build_test_server_with_db(Arc::new(common::in_memory_db().await), settings.clone()).await;

    // No session at all: whatever the reports panels do, the compare panels do.
    let reports = server.get("/admin/reports/panels").await;
    let compare = server.get("/admin/compare/panels").await;
    assert_eq!(compare.status_code(), reports.status_code());
    assert_eq!(
        compare.headers().get("location"),
        reports.headers().get("location"),
        "unauthenticated compare panels must go where reports panels go"
    );

    // A token with a role the dashboard does not recognise gets the same answer
    // from both pages.
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    let odd = issue_jwt(
        &AdminClaims { sub: 7, name: "odd".to_string(), role: "nobody".to_string(), exp },
        &settings.auth.jwt_secret,
    )
    .unwrap();
    let reports = server
        .get("/admin/reports")
        .add_header(session_cookie(&odd).0, session_cookie(&odd).1)
        .await;
    let compare = server
        .get("/admin/compare")
        .add_header(session_cookie(&odd).0, session_cookie(&odd).1)
        .await;
    assert_eq!(compare.status_code(), reports.status_code());

    // A forged token is rejected the same way.
    let reports = server
        .get("/admin/reports")
        .add_header(session_cookie("garbage").0, session_cookie("garbage").1)
        .await;
    let compare = server
        .get("/admin/compare")
        .add_header(session_cookie("garbage").0, session_cookie("garbage").1)
        .await;
    assert_eq!(compare.status_code(), reports.status_code());
    assert_eq!(compare.headers().get("location"), reports.headers().get("location"));
}

#[tokio::test]
async fn compare_escapes_tag_values_and_chart_json_round_trips() {
    let raw_db = common::in_memory_db().await;
    let hostile = "<b>x</b> \"quoted\" a&b";
    let tags = serde_json::json!({ "arm": hostile }).to_string();
    seed_compare_ledger(&raw_db, "mock-model", "mock", &tags, 2).await;
    seed_compare_ledger(&raw_db, "mock-model-b", "mock", r#"{"arm":"plain"}"#, 1).await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    // The picker lists the hostile value, escaped.
    let page = server
        .get("/admin/compare")
        .add_query_param("dimension", "tag")
        .add_query_param("key", "arm")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(page.status_code(), 200);
    let body = page.text();
    assert!(!body.contains("<b>x</b>"), "raw markup from a tag value leaked into the page: {body}");
    assert!(
        body.contains("&lt;b&gt;x&lt;&#x2f;b&gt;") || body.contains("&lt;b&gt;x&lt;/b&gt;"),
        "escaped value missing: {body}"
    );

    let panels = server
        .get("/admin/compare/panels")
        .add_query_param("dimension", "tag")
        .add_query_param("key", "arm")
        .add_query_param("a", hostile)
        .add_query_param("b", "plain")
        .add_query_param("window", "all")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;
    assert_eq!(panels.status_code(), 200, "{}", panels.text());
    let body = panels.text();
    assert!(!body.contains("<b>x</b>"), "raw markup leaked into the panels: {body}");
    let charts = chart_data_attrs(&body);
    let bars: serde_json::Value =
        serde_json::from_str(&html_unescape(&charts["compare-bars-chart"])).unwrap();
    assert_eq!(bars["a"]["label"], hostile, "chart JSON must round-trip the original value");
}
