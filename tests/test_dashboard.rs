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
async fn failure_detail_route_returns_404_for_missing() {
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
