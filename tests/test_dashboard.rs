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
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
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
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
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
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
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

// ── Experiments page ──────────────────────────────────────────────────────────

/// A superadmin session whose actor exists in `admin_users`, so the audit
/// rows the page writes have someone to reference.
async fn superadmin_jwt(db: &modelrouter::db::sqlite::SqliteDb, settings: &Settings) -> String {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let admin = AdminUserRepository::create(
        db,
        NewAdminUser {
            name: "super-user".to_string(),
            password_hash: "x".to_string(),
            role: "superadmin".to_string(),
        },
    )
    .await
    .unwrap();
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    let claims = AdminClaims {
        sub: admin.id,
        name: admin.name,
        role: "superadmin".to_string(),
        exp,
    };
    issue_jwt(&claims, &settings.auth.jwt_secret).unwrap()
}

/// Create an experiment through the REST API, the way an operator without the
/// dashboard would, and return its id.
async fn create_experiment_via_api(
    server: &TestServer,
    token: &str,
    body: &serde_json::Value,
) -> i64 {
    let res = server
        .post("/admin/api/experiments")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        )
        .json(body)
        .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    res.json::<serde_json::Value>()["id"].as_i64().unwrap()
}

/// Two priced, configured targets that need no alias.
fn experiment_body(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "variants": {
            "control": { "fast": "openai/gpt-4o-mini" },
            "candidate": { "fast": "anthropic/claude-haiku-4-5" }
        },
        "expires_at": 0,
        "content_retention_days": 0,
        "retain_content": false
    })
}

/// One stamped ledger row of `run` under `variant`, for the results panel.
async fn seed_experiment_ledger(
    db: &modelrouter::db::sqlite::SqliteDb,
    experiment: i64,
    run: &str,
    variant: &str,
) {
    use modelrouter::db::models::{NewCostLedgerEntry, NewUser};
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::users::UserRepository;

    if UserRepository::find_by_name(db, "exp-user").await.unwrap().is_none() {
        UserRepository::create(db, NewUser { name: "exp-user".to_string(), email: None })
            .await
            .unwrap();
    }
    let user = UserRepository::find_by_name(db, "exp-user").await.unwrap().unwrap();
    CostRepository::create(
        db,
        NewCostLedgerEntry {
            user_id: user.id,
            prompt_id: None,
            model: "openai/gpt-4o-mini".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.01,
            api_key_id: None,
            attribution_correlation_id: Some(run.to_string()),
            attribution_tags: "{}".to_string(),
            experiment_id: Some(experiment),
            experiment_variant: Some(variant.to_string()),
            tokens_estimated: false,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn experiments_page_lists_a_created_experiment_with_its_status() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let viewer = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;
    create_experiment_via_api(&server, &admin, &experiment_body("haiku-vs-mini")).await;

    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    assert_eq!(page.status_code(), 200, "{}", page.text());
    let body = page.text();
    assert!(body.contains("href=\"/admin/experiments\""), "nav link missing: {body}");
    assert!(body.contains("haiku-vs-mini"), "created experiment missing: {body}");
    assert!(body.contains("tag-enabled\">active<"), "status column missing: {body}");
    assert!(body.contains("<code>control</code>") && body.contains("<code>candidate</code>"));
    assert!(body.contains("<td>never</td>"), "an expiry of 0 must render as never: {body}");
    assert!(!body.contains("No experiments yet."));
}

#[tokio::test]
async fn experiments_panels_render_variant_cards_and_run_rows() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let viewer = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;
    let id = create_experiment_via_api(&server, &admin, &experiment_body("panels")).await;
    seed_experiment_ledger(&raw_db, id, "run-a", "control").await;
    seed_experiment_ledger(&raw_db, id, "run-a", "control").await;
    seed_experiment_ledger(&raw_db, id, "run-b", "candidate").await;

    let panels = server
        .get(&format!("/admin/experiments/{id}/panels"))
        .add_query_param("limit", "50")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    assert_eq!(panels.status_code(), 200, "{}", panels.text());
    let body = panels.text();
    assert_eq!(body.matches("variant-card").count(), 2, "one card per variant: {body}");
    assert!(body.contains("<code>control</code>") && body.contains("<code>candidate</code>"));
    assert_eq!(body.matches("class=\"run-row\"").count(), 2, "one row per run: {body}");
    assert!(body.contains("<code>run-a</code>") && body.contains("<code>run-b</code>"));
    assert!(body.contains("exp-user"), "runs must name the user: {body}");
    assert!(body.contains("no samples"), "no prompt rows means no latency samples: {body}");
    assert!(body.contains("computed "), "the panel header must show computed_at: {body}");
    assert!(body.contains("1–2 of 2"), "paging must show the total: {body}");
    assert!(body.contains("gpt-4o-mini"), "the per-model table must list the model: {body}");

    // Paging: one run per page, and the second page links back.
    let second = server
        .get(&format!("/admin/experiments/{id}/panels"))
        .add_query_param("limit", "1")
        .add_query_param("offset", "1")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    let body = second.text();
    assert_eq!(body.matches("class=\"run-row\"").count(), 1, "{body}");
    assert!(body.contains("2–2 of 2"), "{body}");
    let plain = html_unescape(&body);
    assert!(plain.contains(&format!("/admin/experiments/{id}/panels?limit=1&offset=0")), "{plain}");

    // Out-of-range paging is refused inline, in the panel, naming the field.
    let bad = server
        .get(&format!("/admin/experiments/{id}/panels"))
        .add_query_param("limit", "0")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    assert_eq!(bad.status_code(), 200);
    assert!(bad.text().contains("limit must be"), "{}", bad.text());
}

#[tokio::test]
async fn experiments_page_badges_a_retaining_experiment_with_its_window() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let viewer = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;
    let mut body = experiment_body("retaining");
    body["expires_at"] = serde_json::json!("2999-01-01T00:00:00Z");
    body["retain_content"] = serde_json::json!(true);
    body["content_retention_days"] = serde_json::json!(30);
    create_experiment_via_api(&server, &admin, &body).await;

    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    let html = page.text();
    assert!(html.contains("retains content · 30 days"), "badge with window missing: {html}");
    assert!(html.contains("2999-01-01 00:00:00"), "a dated expiry must be rendered: {html}");
}

#[tokio::test]
async fn experiments_form_without_an_expiry_is_rejected_inline_and_the_list_is_unchanged() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let variants = experiment_body("x")["variants"].to_string();
    let res = server
        .post("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .form(&[
            ("name", "no-expiry"),
            ("variants", variants.as_str()),
            ("expires_in", ""),
            ("content_retention_days", "0"),
        ])
        .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let body = res.text();
    assert!(body.contains("alert-danger"), "rejection must be an inline alert: {body}");
    assert!(body.contains("expires_at"), "the rejection must name the field: {body}");
    assert!(!body.contains("hx-get=\"/admin/experiments/rows\""), "no refresh on rejection");

    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;
    let html = page.text();
    assert!(html.contains("No experiments yet."), "the list must be unchanged: {html}");
    assert!(!html.contains("no-expiry"));
}

#[tokio::test]
async fn experiments_page_on_an_empty_deployment_renders_the_empty_state_row() {
    let settings = Arc::new(Settings::default());
    let db = Arc::new(common::in_memory_db().await);
    let viewer = viewer_jwt(&settings);
    let superadmin = superadmin_jwt(&db, &settings).await;
    let server = build_test_server_with_db(db, settings).await;

    // A viewer sees the empty list and no create form.
    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    assert_eq!(page.status_code(), 200, "{}", page.text());
    let html = page.text();
    assert!(html.contains("No experiments yet."), "{html}");
    assert!(!html.contains("name=\"expires_in\""), "{html}");

    // A superadmin gets the form; the expiry select has no preselected value,
    // the placeholder comes first, and retention days is required.
    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&superadmin).0, session_cookie(&superadmin).1)
        .await;
    assert_eq!(page.status_code(), 200, "{}", page.text());
    let html = page.text();
    assert!(html.contains("No experiments yet."), "{html}");
    assert!(html.contains("name=\"expires_in\" required"), "{html}");
    assert!(html.contains("<option value=\"\">Choose…</option>"), "{html}");
    assert!(html.contains("name=\"content_retention_days\" required"), "{html}");
}

#[tokio::test]
async fn experiments_close_button_carries_a_confirm_naming_the_experiment() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let viewer = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;
    create_experiment_via_api(&server, &admin, &experiment_body("closable")).await;

    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;
    let html = page.text();
    let confirm = html
        .split("hx-confirm=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the Close button must carry hx-confirm");
    assert!(confirm.contains("closable"), "confirm must name the experiment: {confirm}");
    assert!(confirm.contains("retention clock"), "confirm must mention the retention clock: {confirm}");
    assert!(html.contains("hx-post=\"/admin/experiments/1/close\""), "{html}");

    // A viewer sees the row but not the Close button.
    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    let html = page.text();
    assert!(html.contains("closable"));
    assert!(!html.contains("hx-confirm"), "viewers cannot close: {html}");
}

#[tokio::test]
async fn a_viewer_session_cannot_create_an_experiment_from_the_page() {
    let settings = Arc::new(Settings::default());
    let viewer = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(common::in_memory_db().await), settings).await;

    let variants = experiment_body("x")["variants"].to_string();
    let res = server
        .post("/admin/experiments")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .form(&[
            ("name", "viewer-made"),
            ("variants", variants.as_str()),
            ("expires_in", "never"),
            ("content_retention_days", "0"),
        ])
        .await;
    assert_eq!(res.status_code(), 403);

    let close = server
        .post("/admin/experiments/1/close")
        .add_header(session_cookie(&viewer).0, session_cookie(&viewer).1)
        .await;
    assert_eq!(close.status_code(), 403);
}

#[tokio::test]
async fn experiments_form_creates_and_closes_through_the_page() {
    use modelrouter::db::models::NewUser;
    use modelrouter::db::repositories::audit::AuditRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let alice = UserRepository::create(&*raw_db, NewUser { name: "alice".to_string(), email: None })
        .await
        .unwrap();
    let bob = UserRepository::create(&*raw_db, NewUser { name: "bob".to_string(), email: None })
        .await
        .unwrap();
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let variants = experiment_body("x")["variants"].to_string();
    let alice_id = alice.id.to_string();
    let bob_id = bob.id.to_string();
    let res = server
        .post("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .form(&[
            ("name", "from-the-form"),
            ("variants", variants.as_str()),
            ("expires_in", "7d"),
            ("content_retention_days", "14"),
            ("retain_content", "on"),
            ("allowed_user_ids", alice_id.as_str()),
            ("allowed_user_ids", bob_id.as_str()),
        ])
        .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let body = res.text();
    assert!(body.contains("from-the-form") && !body.contains("alert-danger"), "{body}");
    assert!(body.contains("hx-get=\"/admin/experiments/rows\""), "success must refresh the list: {body}");

    // The rows fragment the refresh loads shows the new row with its badge and users.
    let rows = server
        .get("/admin/experiments/rows")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;
    assert_eq!(rows.status_code(), 200);
    let html = rows.text();
    assert!(html.starts_with("\n") || html.starts_with("<tr") || html.trim_start().starts_with("<tr"), "{html}");
    assert!(!html.contains("<html"), "the fragment must not be the whole page: {html}");
    assert!(html.contains("from-the-form"), "{html}");
    assert!(html.contains("retains content · 14 days"), "{html}");
    assert!(html.contains("alice, bob"), "{html}");

    // A relative expiry became a dated one, seven days out.
    let stored = server
        .get("/admin/api/experiments/1")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {admin}")).unwrap(),
        )
        .await
        .json::<serde_json::Value>();
    let expires_at = stored["expires_at"].as_i64().unwrap();
    let seven_days = (chrono::Utc::now() + chrono::Duration::days(7)).timestamp();
    assert!((expires_at - seven_days).abs() < 60, "expires_at {expires_at} vs {seven_days}");
    assert_eq!(stored["allowed_user_ids"], serde_json::json!([alice.id, bob.id]));

    // A duplicate name is refused inline, naming the field.
    let dup = server
        .post("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .form(&[
            ("name", "from-the-form"),
            ("variants", variants.as_str()),
            ("expires_in", "never"),
            ("content_retention_days", "0"),
        ])
        .await;
    assert!(dup.text().contains("alert-danger") && dup.text().contains("name"), "{}", dup.text());

    // Malformed variants JSON is refused before validation, naming the field.
    let bad_json = server
        .post("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .form(&[
            ("name", "bad-json"),
            ("variants", "{not json"),
            ("expires_in", "never"),
            ("content_retention_days", "0"),
        ])
        .await;
    assert!(bad_json.text().contains("alert-danger") && bad_json.text().contains("variants"), "{}", bad_json.text());

    // Close through the page: success notice plus refresh, then the row is closed.
    let closed = server
        .post("/admin/experiments/1/close")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;
    assert_eq!(closed.status_code(), 200, "{}", closed.text());
    let body = closed.text();
    assert!(body.contains("closed") && body.contains("hx-get=\"/admin/experiments/rows\""), "{body}");
    let again = server
        .post("/admin/experiments/1/close")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;
    assert!(again.text().contains("alert-danger") && again.text().contains("already closed"), "{}", again.text());

    let page = server
        .get("/admin/experiments")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;
    let html = page.text();
    assert!(html.contains("tag-disabled\">closed<"), "{html}");
    assert!(!html.contains("hx-confirm"), "a closed experiment has no Close button: {html}");

    // Both writes were audited under the session's actor.
    let entries = AuditRepository::list(&*raw_db, 10, 0).await.unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"experiment.create"), "{actions:?}");
    assert!(actions.contains(&"experiment.close"), "{actions:?}");
    assert!(entries.iter().all(|e| e.actor_name == "super-user"), "{entries:?}");
}

#[tokio::test]
async fn failure_detail_route_returns_200_for_existing() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;
    let created = FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "anthropic/claude-sonnet-4".to_string(),
            routed_model: Some("claude-sonnet-4".to_string()),
            provider: Some("anthropic".to_string()),
            stage: FailureStage::Provider,
            status_code: Some(429),
            error_message: "rate_limit_error: Rate limit exceeded".to_string(),
            attempts: 2,
            latency_ms: Some(123),
            project: None,
            attribution_correlation_id: Some("test-correlation-123".to_string()),
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
        .get(&format!("/admin/failures/{}", created.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "failure detail should render for an existing failure");
    let body = resp.text();
    assert!(
        body.contains("claude-sonnet-4"),
        "detail must show routed model"
    );
    assert!(
        body.contains("rate_limit_error"),
        "detail must show error message"
    );
    assert!(
        body.contains("test-correlation-123"),
        "detail must show correlation id"
    );
    assert!(
        body.contains("429"),
        "detail must show status code"
    );
}

#[tokio::test]
async fn failure_detail_route_returns_200_with_message_for_missing() {
    // Route deliberately returns 200 + "not found" message rather than 404,
    // following the dashboard pattern of rendering a friendly message in the
    // existing layout instead of an error page.
    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures/99999")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "route returns 200 even when not found");
    let body = resp.text();
    assert!(
        body.contains("not found"),
        "body should indicate failure not found"
    );
}

#[tokio::test]
async fn failures_list_filters_by_correlation_id() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;

    // Create failure with correlation id "match-this"
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "model-a".to_string(),
            routed_model: None,
            provider: None,
            stage: FailureStage::Resolve,
            status_code: None,
            error_message: "First failure".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some("match-this".to_string()),
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
        },
    )
    .await
    .expect("first failure should persist");

    // Create failure with different correlation id
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "model-b".to_string(),
            routed_model: None,
            provider: None,
            stage: FailureStage::Provider,
            status_code: None,
            error_message: "Second failure".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some("different-id".to_string()),
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
        },
    )
    .await
    .expect("second failure should persist");

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/failures")
        .add_query_param("correlation_id", "match-this")
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", token)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "filtered failures page should render");
    let body = resp.text();
    assert!(
        body.contains("First failure"),
        "response must include the matching failure"
    );
    assert!(
        !body.contains("Second failure"),
        "response must NOT include non-matching failure"
    );
}

#[tokio::test]
async fn failures_list_shows_correlation_id_column() {
    use modelrouter::db::models::{FailureStage, NewRequestFailure};
    use modelrouter::db::repositories::failures::FailureRepository;

    let raw_db = common::in_memory_db().await;
    FailureRepository::create(
        &raw_db,
        NewRequestFailure {
            user_id: None,
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "test-model".to_string(),
            routed_model: None,
            provider: None,
            stage: FailureStage::Resolve,
            status_code: None,
            error_message: "Test error".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some("visible-correlation-id".to_string()),
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

    assert_eq!(resp.status_code(), 200, "failures list should render");
    let body = resp.text();
    assert!(
        body.contains("visible-correlation-id"),
        "correlation id must appear in the list"
    );
    assert!(
        body.contains("Correlation ID"),
        "table header for correlation id must be present"
    );
}

// ── Generate API key tests ────────────────────────────────────────────────────

/// Superadmin can generate a key for a user with email, and the response includes
/// the raw key and a mailto link.
#[tokio::test]
async fn superadmin_generate_key_with_email() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::NewUser;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(
        &raw_db,
        NewUser {
            name: "alice".to_string(),
            email: Some("alice@example.com".to_string()),
        },
    )
    .await
    .unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .post(&format!("/admin/users/{}/keys/generate", user.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", admin)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200, "superadmin should be able to generate key");
    let body = resp.text();

    assert!(body.contains("mr-"), "response must contain raw key starting with mr-");
    assert!(body.contains("Key Generated"), "response must show key generation success message");
    assert!(body.contains("mailto:"), "response must contain mailto link for user with email");
    assert!(body.contains("alice@example.com"), "mailto link must include user email");
    assert!(body.contains("Your%20ModelRouter%20API%20Key"), "subject must be URL-encoded");
    assert!(!body.contains("Your ModelRouter API Key") || body.contains("Your%20ModelRouter%20API%20Key"),
        "mailto subject should be URL-encoded, not contain raw spaces in the URL");
}

/// User without email shows key but no mailto link.
#[tokio::test]
async fn superadmin_generate_key_no_email() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::NewUser;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(
        &raw_db,
        NewUser {
            name: "bob".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .post(&format!("/admin/users/{}/keys/generate", user.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", admin)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();

    assert!(body.contains("mr-"), "response must contain raw key");
    assert!(body.contains("Key Generated"), "response must show success message");
    assert!(!body.contains("mailto:"), "response must not contain mailto link when user has no email");
    assert!(body.contains("No email on file"), "response must indicate no email available");
}

/// Viewer session cannot generate keys (superadmin only).
#[tokio::test]
async fn viewer_cannot_generate_key() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::NewUser;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(
        &raw_db,
        NewUser {
            name: "charlie".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let viewer = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .post(&format!("/admin/users/{}/keys/generate", user.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", viewer)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 403, "viewer role should get 403 when trying to generate key");
}

/// The generated key is stored as a hash, not raw text.
#[tokio::test]
async fn generated_key_stored_as_hash() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::models::NewUser;
    use modelrouter::api::auth::hash_token;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(
        &raw_db,
        NewUser {
            name: "dave".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db.clone()), settings).await;

    let resp = server
        .post(&format!("/admin/users/{}/keys/generate", user.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", admin)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();

    let raw_key_start = body.find("mr-").expect("response must contain raw key");
    let raw_key_fragment = &body[raw_key_start..];
    let raw_key_end = raw_key_fragment.find("</code>").expect("key must be in code block");
    let raw_key = &raw_key_fragment[..raw_key_end];

    assert!(raw_key.starts_with("mr-"), "extracted key must start with mr-");
    assert!(raw_key.len() > 10, "key must have reasonable length");

    let keys = ApiKeyRepository::list_api_keys_for_user(&raw_db, user.id)
        .await
        .expect("should be able to list keys");

    assert_eq!(keys.len(), 1, "exactly one key should be created");
    let stored_key = &keys[0];

    assert_ne!(stored_key.key_hash, raw_key, "stored hash must not equal raw key");

    let expected_hash = hash_token(raw_key);
    assert_eq!(stored_key.key_hash, expected_hash, "stored hash must match SHA-256 of raw key");
}

// ── User row escaping (issue #75) ──────────────────────────────────────

#[tokio::test]
async fn user_row_escapes_name_and_email() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::NewUser;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(
        &raw_db,
        NewUser {
            name: "<script>alert(1)</script>".to_string(),
            email: Some("a@b<img src=x>".to_string()),
        },
    )
    .await
    .unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    // The enable endpoint returns the user_row_html fragment.
    let resp = server
        .post(&format!("/admin/users/{}/enable", user.id))
        .add_header(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_str(&format!("mr_admin_session={}", admin)).unwrap(),
        )
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(
        body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "name must be HTML-escaped in the row fragment; got: {body}"
    );
    assert!(
        !body.contains("<script>alert(1)</script>"),
        "raw name markup must not appear in the row fragment"
    );
    assert!(
        body.contains("a@b&lt;img"),
        "email must be HTML-escaped in the row fragment; got: {body}"
    );
    assert!(!body.contains("<img src=x>"), "raw email markup must not appear");
}

// ── Overview page ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn overview_page_renders_spend_totals() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::models::{NewUser, NewCostLedgerEntry};

    let raw_db = common::in_memory_db().await;
    let user = UserRepository::create(&raw_db, NewUser { name: "alice".to_string(), email: None })
        .await.unwrap();

    // Seed cost data
    CostRepository::create(&raw_db, NewCostLedgerEntry {
        user_id: user.id,
        prompt_id: None,
        model: "test-model".to_string(),
        provider: "test".to_string(),
        project: None,
        tokens_in: 10,
        tokens_out: 5,
        cost_usd: 1.23,
        api_key_id: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".to_string(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    }).await.unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("Spend Today") || body.contains("spend_today"), "overview must show spend metrics: {body}");
}

// ── Prompts page ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn prompts_page_paging_works() {
    use modelrouter::db::repositories::prompts::PromptRepository;
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::{NewPrompt, NewUser};

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    // Create a user first (for foreign key)
    let user = UserRepository::create(&raw_db, NewUser {
        name: "test-user".to_string(),
        email: None,
    }).await.unwrap();

    // Create test prompts
    for i in 0..3 {
        PromptRepository::create(&raw_db, NewPrompt {
            user_id: user.id,
            session_id: None,
            request_model: format!("model-{}", i),
            routed_model: format!("routed-{}", i),
            provider: "test".to_string(),
            messages: "[]".to_string(),
            response: Some("test".to_string()),
            cost_usd: 0.01,
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: Some("stop".to_string()),
            latency_ms: Some(100),
            ttft_ms: None,
            attempts: None,
            tags: "{}".to_string(),
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
        }).await.unwrap();
    }

    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/prompts")
        .add_query_param("page", "1")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("model-"), "prompts page must list prompts");
}

#[tokio::test]
async fn prompt_detail_route_returns_content() {
    use modelrouter::db::repositories::prompts::PromptRepository;
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::{NewPrompt, NewUser};

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    // Create a user first (for foreign key)
    let user = UserRepository::create(&raw_db, NewUser {
        name: "test-user".to_string(),
        email: None,
    }).await.unwrap();

    let prompt = PromptRepository::create(&raw_db, NewPrompt {
        user_id: user.id,
        session_id: None,
        request_model: "test-model".to_string(),
        routed_model: "routed".to_string(),
        provider: "test".to_string(),
        messages: r#"[{"role":"user","content":"hello"}]"#.to_string(),
        response: Some("world".to_string()),
        cost_usd: 0.01,
        prompt_tokens: 10,
        completion_tokens: 5,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: Some("stop".to_string()),
        latency_ms: Some(100),
        ttft_ms: None,
        attempts: None,
        tags: "{}".to_string(),
        project: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".to_string(),
        experiment_id: None,
        experiment_variant: None,
    }).await.unwrap();

    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get(&format!("/admin/prompts/{}", prompt.id))
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("hello"), "prompt detail must show messages: {body}");
    assert!(body.contains("world"), "prompt detail must show response");
}

// ── Storage settings ──────────────────────────────────────────────────────────

#[tokio::test]
async fn storage_settings_persist_through_post() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = server
        .post("/admin/storage-settings")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .form(&[
            ("store_prompts", "on"),
            ("store_prompt_content", "on"),
            ("prompt_retention_days", "90"),
        ])
        .await;

    assert_eq!(resp.status_code(), 303, "storage settings should redirect after save");
}

// ── Cost page ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cost_page_renders_with_filters() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::models::NewUser;

    let raw_db = common::in_memory_db().await;
    UserRepository::create(&raw_db, NewUser { name: "alice".to_string(), email: None }).await.unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/cost")
        .add_query_param("window", "monthly")
        .add_query_param("user", "alice")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("cost") || body.contains("Cost"), "cost page must render: {body}");
}

// ── Audit page ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn audit_page_lists_recent_actions() {
    use modelrouter::db::repositories::audit::AuditRepository;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;
    use modelrouter::db::models::{NewAuditLogEntry, NewAdminUser};

    let raw_db = common::in_memory_db().await;

    // Create an admin user first (for foreign key)
    let admin = AdminUserRepository::create(&raw_db, NewAdminUser {
        name: "test-admin".to_string(),
        password_hash: "x".to_string(),
        role: "viewer".to_string(),
    }).await.unwrap();

    AuditRepository::create(&raw_db, NewAuditLogEntry {
        actor_id: Some(admin.id),
        actor_name: "test-admin".to_string(),
        action: "test.action".to_string(),
        target: Some("resource:123".to_string()),
        before_json: None,
        after_json: None,
    }).await.unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/audit")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("test.action"), "audit page must list actions: {body}");
}

// ── Admins page ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn admins_page_superadmin_only() {
    use modelrouter::db::repositories::admin_users::AdminUserRepository;
    use modelrouter::db::models::NewAdminUser;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    AdminUserRepository::create(&raw_db, NewAdminUser {
        name: "test-admin".to_string(),
        password_hash: "x".to_string(),
        role: "superadmin".to_string(),
    }).await.unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/admins")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("test-admin") || body.contains("super-user"), "admins page must list admin users");
}

#[tokio::test]
async fn create_admin_through_dashboard() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = server
        .post("/admin/admins")
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .form(&[
            ("name", "new-admin"),
            ("password", "test-password-456"),
            ("role", "viewer"),
        ])
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("new-admin"), "response must confirm admin creation: {body}");
}

#[tokio::test]
async fn delete_admin_cannot_delete_self() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let _admin = superadmin_jwt(&raw_db, &settings).await;

    // Get the superadmin's ID from the JWT claims
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    let claims = AdminClaims {
        sub: 1,
        name: "super-user".to_string(),
        role: "superadmin".to_string(),
        exp,
    };
    let token = issue_jwt(&claims, &settings.auth.jwt_secret).unwrap();

    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = server
        .post("/admin/admins/1/delete")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("Cannot delete yourself"), "must prevent self-deletion: {body}");
}

// ── Hooks page ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn hooks_page_renders_empty_without_postgres() {
    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/hooks")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200, "hooks page should render even without pool");
}

// ── Create user (dashboard form handlers) ────────────────────────────────────

// Note: User creation via dashboard uses post_create_user which requires a valid name.
// Testing the validation error path for empty names.

// ── Keys page ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn keys_page_lists_all_api_keys() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::models::{NewUser, NewApiKey};
    use modelrouter::api::auth::hash_token;

    let raw_db = common::in_memory_db().await;
    let user = UserRepository::create(&raw_db, NewUser {
        name: "bob".to_string(),
        email: None,
    }).await.unwrap();

    ApiKeyRepository::create_api_key(&raw_db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("test-key"),
        label: Some("test-label".to_string()),
        expires_at: None,
        project: Some("test-project".to_string()),
        session_window_secs: None,
    }).await.unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/keys")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("bob"), "keys page must list user: {body}");
    assert!(body.contains("test-project"), "keys page must list project");
}

// Note: Key creation form validation happens in dashboard handlers

#[tokio::test]
async fn rotate_key_disables_old_and_creates_new() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::models::{NewUser, NewApiKey};
    use modelrouter::api::auth::hash_token;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(&raw_db, NewUser {
        name: "rotate-user".to_string(),
        email: Some("rotate@test.com".to_string()),
    }).await.unwrap();

    let old_key = ApiKeyRepository::create_api_key(&raw_db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("old-key"),
        label: Some("old".to_string()),
        expires_at: None,
        project: Some("test".to_string()),
        session_window_secs: None,
    }).await.unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db.clone()), settings).await;

    let resp = server
        .post(&format!("/admin/keys/{}/rotate", old_key.id))
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("mr-"), "rotate must show new raw key: {body}");

    // Old key should be disabled
    let keys = ApiKeyRepository::list_api_keys_for_user(&raw_db, user.id).await.unwrap();
    let disabled = keys.iter().filter(|k| !k.enabled).count();
    assert_eq!(disabled, 1, "old key must be disabled");
}

#[tokio::test]
async fn disable_key_marks_key_disabled() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::models::{NewUser, NewApiKey};
    use modelrouter::api::auth::hash_token;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let user = UserRepository::create(&raw_db, NewUser {
        name: "disable-user".to_string(),
        email: None,
    }).await.unwrap();

    let key = ApiKeyRepository::create_api_key(&raw_db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("disable-key"),
        label: None,
        expires_at: None,
        project: None,
        session_window_secs: None,
    }).await.unwrap();

    let admin = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(Arc::new(raw_db.clone()), settings).await;

    let resp = server
        .post(&format!("/admin/keys/{}/disable", key.id))
        .add_header(session_cookie(&admin).0, session_cookie(&admin).1)
        .await;

    assert_eq!(resp.status_code(), 200);

    let keys = ApiKeyRepository::list_api_keys_for_user(&raw_db, user.id).await.unwrap();
    assert!(!keys[0].enabled, "key must be disabled");
}

// ── DashboardError variants ───────────────────────────────────────────────────

#[tokio::test]
async fn dashboard_error_template_renders() {
    let server = build_test_server().await;

    // Force a template error by requesting a nonexistent endpoint that triggers BadRequest
    let resp = server.get("/admin/compare/panels").await;

    // Without auth, should redirect
    assert_eq!(resp.status_code(), 303);
}

#[tokio::test]
async fn dashboard_error_not_found_renders() {
    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    // Request a prompt that doesn't exist
    let resp = server
        .get("/admin/prompts/99999")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("not found"), "missing prompt should say not found");
}

// ── Logout ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_logout_clears_cookie() {
    let server = build_test_server().await;
    let resp = server.post("/admin/logout").await;

    assert_eq!(resp.status_code(), 303);
    let set_cookie = resp.headers().get("set-cookie");
    assert!(set_cookie.is_some());
    let cookie_str = set_cookie.unwrap().to_str().unwrap();
    assert!(cookie_str.contains("Max-Age=0"), "logout must clear cookie: {cookie_str}");
}

// ── Login failures ────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_login_invalid_credentials() {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    AdminUserRepository::create(&raw_db, NewAdminUser {
        name: "testuser".to_string(),
        password_hash: bcrypt::hash("correct", 4).unwrap(),
        role: "viewer".to_string(),
    }).await.unwrap();

    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .post("/admin/login")
        .form(&[("username", "testuser"), ("password", "wrong")])
        .await;

    assert_eq!(resp.status_code(), 200, "invalid password should re-render login");
    let body = resp.text();
    assert!(body.contains("invalid credentials") || body.contains("error"), "should show error: {body}");
}

#[tokio::test]
async fn post_login_disabled_account() {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let raw_db = common::in_memory_db().await;
    let settings = Arc::new(Settings::default());

    let admin = AdminUserRepository::create(&raw_db, NewAdminUser {
        name: "disabled".to_string(),
        password_hash: bcrypt::hash("pass", 4).unwrap(),
        role: "viewer".to_string(),
    }).await.unwrap();

    // Disable the account
    AdminUserRepository::set_enabled(&raw_db, admin.id, false).await.unwrap();

    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .post("/admin/login")
        .form(&[("username", "disabled"), ("password", "pass")])
        .await;

    assert_eq!(resp.status_code(), 200, "disabled account should re-render login");
    let body = resp.text();
    assert!(body.contains("disabled") || body.contains("error"), "should show error: {body}");
}

// ── Cost page with filters ────────────────────────────────────────────────────

#[tokio::test]
async fn cost_page_with_project_filter() {
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::models::{NewUser, NewCostLedgerEntry};

    let raw_db = common::in_memory_db().await;
    let user = UserRepository::create(&raw_db, NewUser { name: "cost-user".to_string(), email: None }).await.unwrap();

    CostRepository::create(&raw_db, NewCostLedgerEntry {
        user_id: user.id,
        prompt_id: None,
        model: "test-model".to_string(),
        provider: "test".to_string(),
        project: Some("proj-a".to_string()),
        tokens_in: 10,
        tokens_out: 5,
        cost_usd: 0.05,
        api_key_id: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".to_string(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    }).await.unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = server
        .get("/admin/cost")
        .add_query_param("window", "alltime")
        .add_query_param("project", "proj-a")
        .add_header(session_cookie(&token).0, session_cookie(&token).1)
        .await;

    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("proj-a"), "cost page must show filtered project");
}

// ── Users page: list, create, disable, enable ────────────────────────────────
//
// The users page and its three POST handlers are the dashboard's only path for
// on- and off-boarding a caller. Each handler writes an audit row and returns an
// HTMX row fragment, so the assertions below check the fragment the browser
// swaps in as well as the state change behind it.

/// Seed a server plus a superadmin session over a database the test still holds.
async fn server_with_superadmin() -> (TestServer, Arc<modelrouter::db::sqlite::SqliteDb>, String) {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let token = superadmin_jwt(&raw_db, &settings).await;
    let server = build_test_server_with_db(raw_db.clone(), settings).await;
    (server, raw_db, token)
}

fn with_session<'a>(
    req: axum_test::TestRequest,
    token: &str,
) -> axum_test::TestRequest {
    let (name, value) = session_cookie(token);
    req.add_header(name, value)
}

#[tokio::test]
async fn users_page_lists_seeded_users_with_their_email_and_status() {
    use modelrouter::db::models::NewUser;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = common::in_memory_db().await;
    UserRepository::create(&raw_db, NewUser {
        name: "alice".to_string(),
        email: Some("alice@example.com".to_string()),
    })
    .await
    .unwrap();
    UserRepository::create(&raw_db, NewUser { name: "bob".to_string(), email: None })
        .await
        .unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(Arc::new(raw_db), settings).await;

    let resp = with_session(server.get("/admin/users"), &token).await;
    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("alice"), "the users page must list every user: {body}");
    assert!(body.contains("bob"));
    assert!(body.contains("alice@example.com"), "the email column must be populated");
}

/// A storage fault behind the users page must surface as the generic 500 page,
/// never as a SQL error. Dropping the table under the live server is the cheapest
/// way to make that arm run.
#[tokio::test]
async fn users_page_storage_fault_renders_the_internal_error_page() {
    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    sqlx::query("DROP TABLE users").execute(&raw_db.pool).await.unwrap();

    let resp = with_session(server.get("/admin/users"), &token).await;
    assert_eq!(resp.status_code(), 500);
    let body = resp.text();
    assert!(body.contains("Internal Error"), "got {body}");
    assert!(
        !body.to_lowercase().contains("no such table"),
        "the SQL error must not reach the browser: {body}"
    );
}

#[tokio::test]
async fn create_user_through_the_dashboard_returns_a_row_and_audits() {
    use modelrouter::db::repositories::audit::AuditRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;

    let resp = with_session(server.post("/admin/users"), &token)
        .form(&serde_json::json!({ "name": "  carol  " }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("carol"), "the response is the new row fragment: {body}");

    let users = UserRepository::list(&*db).await.unwrap();
    assert!(
        users.iter().any(|u| u.name == "carol"),
        "the name must be trimmed before it is stored"
    );

    let audit = AuditRepository::list(&*db, 50, 0).await.unwrap();
    assert!(audit.iter().any(|e| e.action == "user.create"), "the create must be audited");
}

/// An all-whitespace name is a form slip, not a user; it must be refused before
/// the row is written.
#[tokio::test]
async fn create_user_with_a_blank_name_is_rejected() {
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;

    let resp = with_session(server.post("/admin/users"), &token)
        .form(&serde_json::json!({ "name": "   " }))
        .await;
    assert_eq!(resp.status_code(), 400);
    assert!(resp.text().contains("name is required"));
    assert!(UserRepository::list(&*db).await.unwrap().is_empty(), "nothing was created");
}

#[tokio::test]
async fn a_viewer_cannot_create_a_user() {
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = with_session(server.post("/admin/users"), &token)
        .form(&serde_json::json!({ "name": "sneaky" }))
        .await;
    assert_eq!(resp.status_code(), 403);
    assert!(UserRepository::list(&*raw_db).await.unwrap().is_empty());
}

/// Disabling a user must also disable their keys — leaving a live key behind
/// would make the dashboard's "Disabled" tag a lie.
#[tokio::test]
async fn disabling_a_user_disables_their_api_keys_and_the_row_flips_to_enable() {
    use modelrouter::db::models::{NewApiKey, NewUser};
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;
    let user = UserRepository::create(&*db, NewUser {
        name: "dana".to_string(),
        email: Some("dana@example.com".to_string()),
    })
    .await
    .unwrap();
    let key = db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: "hash-dana".to_string(),
        label: Some("laptop".to_string()),
        expires_at: None,
        project: Some("proj-a".to_string()),
        session_window_secs: None,
    })
    .await
    .unwrap();

    let resp = with_session(server.post(&format!("/admin/users/{}/disable", user.id)), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("tag-disabled"), "the returned row shows the disabled tag: {body}");
    assert!(body.contains("/enable"), "the toggle button must now offer Enable: {body}");

    assert!(!UserRepository::find_by_id(&*db, user.id).await.unwrap().unwrap().enabled);
    let keys = db.list_api_keys_for_user(user.id).await.unwrap();
    assert!(
        keys.iter().find(|k| k.id == key.id).map(|k| !k.enabled).unwrap_or(false),
        "the user's key must be disabled alongside them"
    );

    // ...and enabling puts the row back.
    let resp = with_session(server.post(&format!("/admin/users/{}/enable", user.id)), &token).await;
    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("tag-enabled"), "got {body}");
    assert!(body.contains("/disable"), "the toggle must offer Disable again: {body}");
    assert!(UserRepository::find_by_id(&*db, user.id).await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn disabling_a_user_that_does_not_exist_is_a_404_fragment() {
    let (server, _db, token) = server_with_superadmin().await;
    let resp = with_session(server.post("/admin/users/424242/disable"), &token).await;
    assert_eq!(resp.status_code(), 404);
    assert!(resp.text().contains("user 424242 not found"));
}

#[tokio::test]
async fn enabling_a_user_that_does_not_exist_is_a_404_fragment() {
    let (server, _db, token) = server_with_superadmin().await;
    let resp = with_session(server.post("/admin/users/424242/enable"), &token).await;
    assert_eq!(resp.status_code(), 404);
    assert!(resp.text().contains("user 424242 not found"));
}

/// Generating a key for a *disabled* user still renders a row, and that row must
/// offer Enable rather than Disable — the fragment is built from the user's live
/// state, not from the assumption that anyone getting a key is active.
#[tokio::test]
async fn generating_a_key_for_a_disabled_user_renders_an_enable_toggle() {
    use modelrouter::db::models::NewUser;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;
    let user = UserRepository::create(&*db, NewUser {
        name: "paused".to_string(),
        email: Some("paused@example.com".to_string()),
    })
    .await
    .unwrap();
    UserRepository::set_enabled(&*db, user.id, false).await.unwrap();

    let resp =
        with_session(server.post(&format!("/admin/users/{}/keys/generate", user.id)), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("tag-disabled"), "got {body}");
    assert!(body.contains("/enable"), "a disabled user's row offers Enable: {body}");
    assert!(body.contains("mr-"), "the one-time raw key is shown: {body}");
}

// ── Keys page: create ────────────────────────────────────────────────────────

/// The create-key form doubles as user creation: naming an unknown user provisions
/// them. The response carries the raw secret exactly once, plus the table fragment
/// the page splices in.
#[tokio::test]
async fn creating_a_key_for_an_unknown_user_provisions_the_user_and_shows_the_secret_once() {
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::audit::AuditRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;

    let resp = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({
            "user_name": " erin ",
            "project": " analytics ",
            "label": " batch ",
            "email": "erin@example.com",
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();

    assert!(body.contains("Key created for"), "got {body}");
    assert!(body.contains("mr-"), "the raw key is shown once: {body}");
    assert!(
        body.contains("Email Key to User") && body.contains("mailto:erin@example.com"),
        "an email on the form yields a mailto button: {body}"
    );
    assert!(body.contains("analytics"), "the project appears in the confirmation: {body}");

    let user = UserRepository::find_by_name(&*db, "erin").await.unwrap().expect("user provisioned");
    assert_eq!(user.email.as_deref(), Some("erin@example.com"));
    let keys = db.list_api_keys_for_user(user.id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].project.as_deref(), Some("analytics"), "the project is trimmed");
    assert_eq!(keys[0].label.as_deref(), Some("batch"));
    assert!(
        !body.contains(&keys[0].key_hash),
        "the stored hash must never be rendered"
    );

    let audit = AuditRepository::list(&*db, 50, 0).await.unwrap();
    assert!(audit.iter().any(|e| e.action == "key.create"));
}

/// With no email anywhere, the confirmation must simply omit the mail button
/// rather than render a `mailto:` with an empty address.
#[tokio::test]
async fn creating_a_key_without_an_email_omits_the_mail_button() {
    let (server, _db, token) = server_with_superadmin().await;

    let resp = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({
            "user_name": "frank",
            "project": "",
            "label": "",
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("Key created for"));
    assert!(!body.contains("Email Key to User"), "no address, no button: {body}");
}

/// A user already on file keeps their stored email even when the form leaves it
/// blank, so the mail button still appears.
#[tokio::test]
async fn creating_a_key_falls_back_to_the_stored_email_of_an_existing_user() {
    use modelrouter::db::models::NewUser;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;
    UserRepository::create(&*db, NewUser {
        name: "gail".to_string(),
        email: Some("gail@example.com".to_string()),
    })
    .await
    .unwrap();

    let resp = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({ "user_name": "gail", "project": "", "label": "", "email": "" }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert!(
        resp.text().contains("mailto:gail@example.com"),
        "the stored address should be used: {}",
        resp.text()
    );
}

/// One key per user+project is the invariant the keys page is built on. A second
/// attempt must return the "already exists" prompt with a rotate button rather
/// than quietly minting a duplicate.
#[tokio::test]
async fn creating_a_second_key_for_the_same_user_and_project_offers_a_rotate_instead() {
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;

    let first = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({ "user_name": "hank", "project": "etl", "label": "" }))
        .await;
    assert_eq!(first.status_code(), 200);

    let second = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({ "user_name": "hank", "project": "etl", "label": "" }))
        .await;
    assert_eq!(second.status_code(), 200, "{}", second.text());
    let body = second.text();
    assert!(body.contains("already exists"), "got {body}");
    assert!(body.contains("Rotate existing key"), "the operator is offered a rotate: {body}");
    assert!(body.contains("tag-enabled"), "the existing key's status is shown: {body}");
    assert!(!body.contains("mr-"), "no second secret was minted: {body}");

    let user = UserRepository::find_by_name(&*db, "hank").await.unwrap().unwrap();
    let keys = db.list_api_keys_for_user(user.id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1, "still exactly one key for this user+project");
}

/// The duplicate check must see a *disabled* key too; otherwise disabling a key
/// would let a second one be minted for the same slot, and the page would show
/// two groups where the model allows one.
#[tokio::test]
async fn the_duplicate_key_check_also_catches_a_disabled_key() {
    use modelrouter::db::models::{NewApiKey, NewUser};
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;
    let user = UserRepository::create(&*db, NewUser { name: "iris".to_string(), email: None })
        .await
        .unwrap();
    let key = db
        .create_api_key(NewApiKey {
            user_id: user.id,
            key_hash: "hash-iris".to_string(),
            label: None,
            expires_at: None,
            project: None,
            session_window_secs: None,
        })
        .await
        .unwrap();
    db.set_key_enabled(key.id, false).await.unwrap();

    let second = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({ "user_name": "iris", "project": "", "label": "" }))
        .await;
    let body = second.text();
    assert!(body.contains("already exists"), "got {body}");
    assert!(body.contains("tag-disabled"), "the existing key is shown as disabled: {body}");
    assert!(body.contains("(no project)"), "an unprojected key is labelled as such: {body}");
    assert!(!body.contains("mr-"), "no second secret was minted: {body}");
}

/// A storage fault during the user lookup must abort the whole create rather
/// than fall through to minting a key against a user that could not be read.
#[tokio::test]
async fn creating_a_key_when_the_user_lookup_faults_is_an_internal_error() {
    let (server, db, token) = server_with_superadmin().await;
    sqlx::query("DROP TABLE users").execute(&db.pool).await.unwrap();

    let resp = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({ "user_name": "ghost", "project": "", "label": "" }))
        .await;
    assert_eq!(resp.status_code(), 500);
    let body = resp.text();
    assert!(body.contains("Internal Error"), "got {body}");
    assert!(!body.to_lowercase().contains("no such table"), "SQL detail leaked: {body}");
}

/// Rotating a key whose owner has no address on file must render the new secret
/// without a mail button — there is nowhere to send it.
#[tokio::test]
async fn rotating_a_key_for_a_user_without_an_email_omits_the_mail_link() {
    use modelrouter::db::models::{NewApiKey, NewUser};
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let (server, db, token) = server_with_superadmin().await;
    let user = UserRepository::create(&*db, NewUser { name: "nomail".to_string(), email: None })
        .await
        .unwrap();
    let key = db
        .create_api_key(NewApiKey {
            user_id: user.id,
            key_hash: "hash-nomail".to_string(),
            label: None,
            expires_at: None,
            project: None,
            session_window_secs: None,
        })
        .await
        .unwrap();

    let resp =
        with_session(server.post(&format!("/admin/keys/{}/rotate", key.id)), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("mr-"), "the new secret is shown: {body}");
    assert!(!body.contains("mailto:"), "no address means no mail link: {body}");
}

#[tokio::test]
async fn creating_a_key_with_a_blank_user_name_is_rejected() {
    let (server, _db, token) = server_with_superadmin().await;
    let resp = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({ "user_name": "  ", "project": "", "label": "" }))
        .await;
    assert_eq!(resp.status_code(), 400);
    assert!(resp.text().contains("user_name is required"));
}

/// A user name carrying HTML must come back escaped in the confirmation banner —
/// the banner interpolates it into markup.
#[tokio::test]
async fn the_key_confirmation_escapes_a_user_name_carrying_markup() {
    let (server, _db, token) = server_with_superadmin().await;
    let resp = with_session(server.post("/admin/keys"), &token)
        .form(&serde_json::json!({
            "user_name": "<script>alert(1)</script>",
            "project": "",
            "label": "",
        }))
        .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(!body.contains("<script>alert(1)</script>"), "unescaped markup in: {body}");
    assert!(body.contains("&lt;script&gt;"), "got {body}");
}

// ── Admins page: delete ──────────────────────────────────────────────────────

#[tokio::test]
async fn deleting_another_admin_removes_the_row_and_audits() {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;
    use modelrouter::db::repositories::audit::AuditRepository;

    let (server, db, token) = server_with_superadmin().await;
    let victim = AdminUserRepository::create(&*db, NewAdminUser {
        name: "retiring-admin".to_string(),
        password_hash: "x".to_string(),
        role: "viewer".to_string(),
    })
    .await
    .unwrap();

    let resp = with_session(server.post(&format!("/admin/admins/{}/delete", victim.id)), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("Deleted"), "got {body}");
    assert!(body.contains(&format!("admin-row-{}", victim.id)), "the row id must match for the swap: {body}");

    assert!(
        AdminUserRepository::find_by_id(&*db, victim.id).await.unwrap().is_none(),
        "the admin is gone"
    );
    let audit = AuditRepository::list(&*db, 50, 0).await.unwrap();
    assert!(audit.iter().any(|e| e.action == "admin.delete"));
}

#[tokio::test]
async fn a_viewer_cannot_delete_an_admin() {
    use modelrouter::db::models::NewAdminUser;
    use modelrouter::db::repositories::admin_users::AdminUserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let victim = AdminUserRepository::create(&*raw_db, NewAdminUser {
        name: "survivor".to_string(),
        password_hash: "x".to_string(),
        role: "viewer".to_string(),
    })
    .await
    .unwrap();
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = with_session(server.post(&format!("/admin/admins/{}/delete", victim.id)), &token).await;
    assert_eq!(resp.status_code(), 403);
    assert!(AdminUserRepository::find_by_id(&*raw_db, victim.id).await.unwrap().is_some());
}

// ── Cost page: the filter matrix ─────────────────────────────────────────────

/// The cost page resolves a user filter and a group filter independently, then
/// intersects them. Exercising every leg at once — with a real group, a real
/// membership and a real key — is what makes the dropdown builders run too.
#[tokio::test]
async fn cost_page_intersects_the_user_and_group_filters() {
    use modelrouter::db::models::{NewApiKey, NewCostLedgerEntry, NewUser};
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::groups::GroupRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let inside = UserRepository::create(&*raw_db, NewUser {
        name: "in-group".to_string(),
        email: Some("in@example.com".to_string()),
    })
    .await
    .unwrap();
    let outside = UserRepository::create(&*raw_db, NewUser {
        name: "out-of-group".to_string(),
        email: None,
    })
    .await
    .unwrap();

    let group = GroupRepository::create_group(&*raw_db, "platform", 10).await.unwrap();
    GroupRepository::add_member(&*raw_db, group.id, inside.id).await.unwrap();
    // A disabled membership must not widen the filter.
    let lapsed = GroupRepository::add_member(&*raw_db, group.id, outside.id).await.unwrap();
    GroupRepository::disable_membership(&*raw_db, lapsed.id).await.unwrap();

    let key = raw_db.create_api_key(NewApiKey {
        user_id: inside.id,
        key_hash: "hash-in-group".to_string(),
        label: Some("ci".to_string()),
        expires_at: None,
        project: Some("pipeline".to_string()),
        session_window_secs: None,
    })
    .await
    .unwrap();
    // A second key with no label and no project, so both display shapes render.
    raw_db.create_api_key(NewApiKey {
        user_id: outside.id,
        key_hash: "hash-out-of-group".to_string(),
        label: None,
        expires_at: None,
        project: None,
        session_window_secs: None,
    })
    .await
    .unwrap();

    for (user_id, model) in [(inside.id, "model-a"), (outside.id, "model-b")] {
        CostRepository::create(&*raw_db, NewCostLedgerEntry {
            user_id,
            prompt_id: None,
            model: model.to_string(),
            provider: "test".to_string(),
            project: Some("pipeline".to_string()),
            tokens_in: 10,
            tokens_out: 5,
            cost_usd: 2.5,
            api_key_id: Some(key.id),
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
        })
        .await
        .unwrap();
    }

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    // Group filter alone: only the active member is in scope.
    let resp = with_session(server.get("/admin/cost").add_query_param("group", "platform"), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("in-group"), "the active member must appear: {body}");
    assert!(
        body.contains("ci") && body.contains("pipeline"),
        "the key dropdown is built from labelled and unlabelled keys: {body}"
    );

    // User ∩ group, where the user is in the group.
    let resp = with_session(
        server
            .get("/admin/cost")
            .add_query_param("group", "platform")
            .add_query_param("user", "in-group"),
        &token,
    )
    .await;
    assert_eq!(resp.status_code(), 200);

    // User ∩ group, where the user is *not* in the group: the intersection is
    // empty and the page must still render rather than fall back to "all".
    let resp = with_session(
        server
            .get("/admin/cost")
            .add_query_param("group", "platform")
            .add_query_param("user", "out-of-group"),
        &token,
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
}

/// A group name that does not exist must narrow to zero rows, not silently widen
/// to every user — that is the difference between "no data" and a data leak.
#[tokio::test]
async fn cost_page_with_an_unknown_group_yields_no_rows() {
    use modelrouter::db::models::{NewCostLedgerEntry, NewUser};
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let user = UserRepository::create(&*raw_db, NewUser {
        name: "solo-spender".to_string(),
        email: None,
    })
    .await
    .unwrap();
    CostRepository::create(&*raw_db, NewCostLedgerEntry {
        user_id: user.id,
        prompt_id: None,
        model: "expensive-model".to_string(),
        provider: "test".to_string(),
        project: None,
        tokens_in: 1000,
        tokens_out: 1000,
        cost_usd: 99.0,
        api_key_id: None,
        attribution_correlation_id: None,
        attribution_tags: "{}".to_string(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp =
        with_session(server.get("/admin/cost").add_query_param("group", "no-such-group"), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert!(
        !resp.text().contains("99"),
        "an unknown group must not fall through to everyone's spend: {}",
        resp.text()
    );
}

/// The remaining filters — project, model, key and window — all reach the same
/// query builder; driving them together keeps the page honest about each one
/// being wired to something.
#[tokio::test]
async fn cost_page_accepts_the_project_model_and_key_filters() {
    use modelrouter::db::models::{NewApiKey, NewCostLedgerEntry, NewUser};
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let user =
        UserRepository::create(&*raw_db, NewUser { name: "jess".to_string(), email: None })
            .await
            .unwrap();
    let key = raw_db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: "hash-jess".to_string(),
        label: None,
        expires_at: None,
        project: Some("reporting".to_string()),
        session_window_secs: None,
    })
    .await
    .unwrap();
    CostRepository::create(&*raw_db, NewCostLedgerEntry {
        user_id: user.id,
        prompt_id: None,
        model: "tracked-model".to_string(),
        provider: "test".to_string(),
        project: Some("reporting".to_string()),
        tokens_in: 7,
        tokens_out: 3,
        cost_usd: 0.75,
        api_key_id: Some(key.id),
        attribution_correlation_id: None,
        attribution_tags: "{}".to_string(),
        experiment_id: None,
        experiment_variant: None,
        tokens_estimated: false,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    for window in ["daily", "weekly", "monthly"] {
        let resp = with_session(
            server
                .get("/admin/cost")
                .add_query_param("window", window)
                .add_query_param("project", "reporting")
                .add_query_param("model", "tracked-model")
                .add_query_param("key_id", key.id),
            &token,
        )
        .await;
        assert_eq!(resp.status_code(), 200, "window={window}: {}", resp.text());
        assert!(
            resp.text().contains("tracked-model"),
            "the model dropdown is built from the ledger: {}",
            resp.text()
        );
    }
}

// ── Overview: budget warnings ────────────────────────────────────────────────

/// The overview's warning band is the only place an operator sees a budget about
/// to bite. It must fire at the 80% mark and name the user, the window and both
/// numbers — and must stay silent for a user comfortably under.
#[tokio::test]
async fn overview_warns_about_a_user_near_their_budget_limit() {
    use modelrouter::db::models::{NewBudgetRule, NewCostLedgerEntry, NewUser};
    use modelrouter::db::repositories::budgets::BudgetRepository;
    use modelrouter::db::repositories::costs::CostRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);

    let spend = |db: Arc<modelrouter::db::sqlite::SqliteDb>, user_id: i64, usd: f64| async move {
        CostRepository::create(&*db, NewCostLedgerEntry {
            user_id,
            prompt_id: None,
            model: "m".to_string(),
            provider: "test".to_string(),
            project: None,
            tokens_in: 1,
            tokens_out: 1,
            cost_usd: usd,
            api_key_id: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
            tokens_estimated: false,
        })
        .await
        .unwrap();
    };

    let rule_for = |user_id: i64, limit: f64, window: &str| NewBudgetRule {
        user_id: Some(user_id),
        group_name: None,
        api_key_id: None,
        tag: None,
        window: window.to_string(),
        limit_usd: Some(limit),
        limit_tokens: None,
        model_allow: vec![],
        model_deny: vec![],
        rate_rpm: None,
        max_concurrent: None,
        project: None,
        window_start: None,
        window_end: None,
    };

    // Over 80% of a monthly limit → warn.
    let hot = UserRepository::create(&*raw_db, NewUser { name: "hot-spender".to_string(), email: None })
        .await
        .unwrap();
    BudgetRepository::create(&*raw_db, rule_for(hot.id, 10.0, "monthly")).await.unwrap();
    spend(raw_db.clone(), hot.id, 9.0).await;

    // Under 80% of a daily limit → stay quiet.
    let cool = UserRepository::create(&*raw_db, NewUser { name: "cool-spender".to_string(), email: None })
        .await
        .unwrap();
    BudgetRepository::create(&*raw_db, rule_for(cool.id, 100.0, "daily")).await.unwrap();
    spend(raw_db.clone(), cool.id, 1.0).await;

    // A weekly rule with a zero limit must never divide-by-zero into a warning.
    let unlimited =
        UserRepository::create(&*raw_db, NewUser { name: "unlimited".to_string(), email: None })
            .await
            .unwrap();
    BudgetRepository::create(&*raw_db, rule_for(unlimited.id, 0.0, "weekly")).await.unwrap();
    spend(raw_db.clone(), unlimited.id, 5.0).await;

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = with_session(server.get("/admin"), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(body.contains("hot-spender"), "the near-limit user must be named: {body}");
    assert!(!body.contains("cool-spender"), "a user at 1% must not be warned about: {body}");
    assert!(!body.contains("unlimited"), "a zero limit is not a breach: {body}");
}

// ── Login failure re-render ──────────────────────────────────────────────────

/// A bad password re-renders the login form carrying the error, rather than
/// redirecting or 500-ing — and must not hand out a session cookie.
#[tokio::test]
async fn a_failed_login_re_renders_the_form_with_an_error() {
    let (server, db, _token) = server_with_superadmin().await;
    let _ = &db;

    let resp = server
        .post("/admin/login")
        .form(&serde_json::json!({ "username": "super-user", "password": "definitely-wrong" }))
        .await;

    assert_eq!(resp.status_code(), 200, "the form comes back, it does not redirect");
    let body = resp.text();
    assert!(body.contains("password") || body.contains("Password"), "got {body}");
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "a failed login must not set a session cookie"
    );
}

#[tokio::test]
async fn a_login_for_an_unknown_admin_re_renders_the_form() {
    let server = build_test_server().await;
    let resp = server
        .post("/admin/login")
        .form(&serde_json::json!({ "username": "nobody-here", "password": "x" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    assert!(resp.headers().get("set-cookie").is_none());
}

// ── Prompts page: user labels ────────────────────────────────────────────────

/// A prompt's author is shown as "name (email)" when an address is on file, so
/// two callers with similar names stay distinguishable.
#[tokio::test]
async fn prompts_page_labels_an_author_with_their_email() {
    use modelrouter::db::models::{NewPrompt, NewUser};
    use modelrouter::db::repositories::prompts::PromptRepository;
    use modelrouter::db::repositories::users::UserRepository;

    let raw_db = Arc::new(common::in_memory_db().await);
    let with_email = UserRepository::create(&*raw_db, NewUser {
        name: "kim".to_string(),
        email: Some("kim@example.com".to_string()),
    })
    .await
    .unwrap();
    let without_email =
        UserRepository::create(&*raw_db, NewUser { name: "lee".to_string(), email: None })
            .await
            .unwrap();

    for uid in [with_email.id, without_email.id] {
        PromptRepository::create(&*raw_db, NewPrompt {
            user_id: uid,
            session_id: None,
            request_model: "m".to_string(),
            routed_model: "m".to_string(),
            provider: "test".to_string(),
            messages: "[]".to_string(),
            response: Some("ok".to_string()),
            finish_reason: Some("stop".to_string()),
            prompt_tokens: 1,
            completion_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.01,
            latency_ms: Some(1),
            ttft_ms: None,
            attempts: None,
            tags: "{}".to_string(),
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
        })
        .await
        .unwrap();
    }

    let settings = Arc::new(Settings::default());
    let token = viewer_jwt(&settings);
    let server = build_test_server_with_db(raw_db.clone(), settings).await;

    let resp = with_session(server.get("/admin/prompts"), &token).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body = resp.text();
    assert!(
        body.contains("kim (kim@example.com)"),
        "an author with an email is labelled with it: {body}"
    );
    assert!(body.contains("lee"), "an author without one falls back to the bare name: {body}");
}

// ── DashboardError rendering ─────────────────────────────────────────────────

/// Every error variant has a distinct status and body. The template arm has no
/// HTTP route that can reach it — the templates are embedded and valid — so it is
/// rendered directly, which is also the only way to pin its status.
#[tokio::test]
async fn every_dashboard_error_variant_renders_its_own_status_and_body() {
    use axum::response::IntoResponse;
    use modelrouter::api::admin::dashboard::DashboardError;

    let cases = [
        (DashboardError::Template("jinja exploded".to_string()), 500, "Template error"),
        (DashboardError::Forbidden, 403, "403 Forbidden"),
        (DashboardError::BadRequest("bad input".to_string()), 400, "Bad Request"),
        (DashboardError::NotFound("gone".to_string()), 404, "Not Found"),
        (DashboardError::Internal, 500, "Internal Error"),
    ];

    for (err, want_status, want_body) in cases {
        let resp = err.into_response();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(status, want_status, "body was {body}");
        assert!(body.contains(want_body), "expected {want_body:?} in {body}");
    }

    // Unauthorized is the odd one out: it redirects rather than rendering.
    let resp = DashboardError::Unauthorized.into_response();
    assert_eq!(resp.status().as_u16(), 303);
    assert_eq!(resp.headers().get("location").unwrap(), "/admin/login");
}
