//! `POST /v1/rerank` end to end: the real route, the real Foundry rerank
//! adapter, and a fake Foundry resource on a loopback port that records what
//! reached it. Nothing here is mocked below the HTTP boundary.
mod common;

use std::sync::{Arc, Mutex};

use axum::{extract::Json, http::HeaderMap, http::StatusCode, http::Uri, response::IntoResponse, routing::post, Router};
use axum_test::TestServer;
use modelrouter::api::app::{build_router_with_registries, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, PricingEntry, ProviderConfig, Settings};
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::extensions::Extensions;
use modelrouter::providers::azure_entra::StaticTokenProvider;
use modelrouter::providers::decision_registry::DecisionRegistry;
use modelrouter::providers::foundry::FoundryRerankAdapter;
use modelrouter::providers::rerank_registry::RerankRegistry;
use modelrouter::providers::{embed_registry::EmbeddingRegistry, registry::ProviderRegistry};
use modelrouter::router::experiments::ExperimentRegistry as LiveExperiments;
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::json;
use std::collections::HashMap;

const DEPLOYMENT: &str = "rerank-a";
const ALIAS: &str = "rerank";
/// Dollars per search unit for `rerank/rerank-a` in the test price table.
const RATE_PER_UNIT: f64 = 0.002;

#[derive(Default)]
struct Capture {
    paths: Vec<String>,
    authorization: Vec<Option<String>>,
    bodies: Vec<serde_json::Value>,
}

/// A fake Foundry resource serving the Cohere rerank path (and an alternate
/// path, for the `rerank_url` override).
async fn spawn_upstream(status: StatusCode, payload: serde_json::Value) -> (String, Arc<Mutex<Capture>>) {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let handler = {
        let capture = capture.clone();
        move |uri: Uri, headers: HeaderMap, body: String| {
            let capture = capture.clone();
            let payload = payload.clone();
            async move {
                let mut c = capture.lock().unwrap();
                c.paths.push(uri.path().to_string());
                c.authorization.push(headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string));
                c.bodies.push(serde_json::from_str(&body).unwrap_or(serde_json::Value::Null));
                (status, Json(payload)).into_response()
            }
        }
    };
    let app = Router::new()
        .route("/providers/cohere/v2/rerank", post(handler.clone()))
        .route("/v2/rerank", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), capture)
}

fn results_payload() -> serde_json::Value {
    json!({
        "id": "rr-1",
        "results": [{"index": 2, "relevance_score": 0.91}, {"index": 0, "relevance_score": 0.22}],
        "meta": {"api_version": {"version": "2"}, "billed_units": {"search_units": 2}}
    })
}

fn request_body() -> serde_json::Value {
    json!({
        "model": ALIAS,
        "query": "how long does onboarding take",
        "documents": ["Pricing starts at $10.", "The vendor's roadmap.", "Onboarding takes two weeks for a mid-size team."],
        "top_n": 2
    })
}

fn foundry_config(endpoint: &str, rerank_deployments: Vec<String>, rerank_url: Option<String>) -> ProviderConfig {
    ProviderConfig { foundry_endpoint: Some(endpoint.to_string()), timeout_secs: 30, rerank_deployments, rerank_url, ..Default::default() }
}

struct TestApp {
    server: TestServer,
    db: Arc<dyn DatabaseProvider>,
}

/// The app with `rerank` aliased to `foundry/rerank-a`, strict resolution on,
/// a flat per-unit price for the deployment (unless `priced` is false), and
/// the real adapter pointed at `endpoint`.
async fn test_app(endpoint: &str, rerank_deployments: Vec<String>, rerank_url: Option<String>, priced: bool) -> TestApp {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let config = foundry_config(endpoint, rerank_deployments, rerank_url);
    let mut settings = Settings::default();
    settings.routing.model_aliases.insert(ALIAS.to_string(), format!("foundry/{DEPLOYMENT}"));
    settings.routing.strict_model_resolution = true;
    settings.routing.default_provider = "foundry".to_string();
    settings.routing.default_model = "foundry/chat-default".to_string();
    settings.providers.insert("foundry".to_string(), config.clone());
    let settings = Arc::new(settings);

    let reranks = Arc::new(RerankRegistry::new(settings.providers.clone()));
    reranks.register(
        "foundry",
        FoundryRerankAdapter::with_token_provider(&config, Arc::new(StaticTokenProvider::new("static-token"))).unwrap(),
    );
    let decisions = Arc::new(DecisionRegistry::new(settings.providers.clone()));

    let prices: Vec<PricingEntry> = if priced {
        vec![PricingEntry { model: format!("rerank/{DEPLOYMENT}"), input_per_million: RATE_PER_UNIT, output_per_million: 0.0, ..Default::default() }]
    } else {
        vec![]
    };
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&prices)),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter { response: "hello".to_string() })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(common::MockEmbeddingAdapter { embedding: vec![0.1] })),
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(HashMap::new())),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(HashMap::new())),
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
        experiments: Arc::new(LiveExperiments::default()),
    };
    let app = build_router_with_registries(state, Extensions::default(), decisions, reranks);
    TestApp { server: TestServer::new(app).unwrap(), db }
}

fn bearer() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static("Bearer test-token"))
}

async fn settle() {
    // The ledger write is fire-and-forget; give a stray write time to land
    // before asserting that none did.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
}

#[tokio::test]
async fn rerank_unauthenticated_returns_401_and_sends_nothing() {
    let (base, capture) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec![], None, true).await;
    let resp = app.server.post("/v1/rerank").json(&request_body()).await;
    assert_eq!(resp.status_code(), 401);
    assert!(capture.lock().unwrap().paths.is_empty());
}

#[tokio::test]
async fn a_rerank_passes_through_the_body_and_the_results_and_prices_the_search_units() {
    let (base, capture) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec![], None, true).await;
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();

    // The provider's results, as they arrived.
    assert_eq!(body["results"][0]["index"], 2);
    assert_eq!(body["results"][0]["relevance_score"].as_f64(), Some(0.91));
    assert_eq!(body["id"], "rr-1");

    // What reached the provider: the Cohere path on the resource root, the
    // caller's body with the resolved deployment.
    {
        let c = capture.lock().unwrap();
        assert_eq!(c.paths, vec!["/providers/cohere/v2/rerank".to_string()]);
        assert_eq!(c.authorization[0].as_deref(), Some("Bearer static-token"));
        assert_eq!(c.bodies[0]["model"], DEPLOYMENT);
        assert_eq!(c.bodies[0]["query"], request_body()["query"]);
        assert_eq!(c.bodies[0]["documents"], request_body()["documents"]);
        assert_eq!(c.bodies[0]["top_n"], 2);
    }

    // Priced per billed search unit, written to the ledger as kind rerank, and
    // reported verbatim in usage and x_router.
    let ledger = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(ledger[0].model, DEPLOYMENT);
    assert_eq!(ledger[0].provider, "foundry");
    assert_eq!(ledger[0].call_kind, "rerank");
    let expected = 2.0 * RATE_PER_UNIT;
    assert!((ledger[0].cost_usd - expected).abs() < 1e-12, "ledger cost {}", ledger[0].cost_usd);
    assert_eq!(body["usage"]["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
    assert_eq!(body["usage"]["search_units"], 2);
    let meta = &body["x_router"];
    assert_eq!(meta["requested_model"], ALIAS);
    assert_eq!(meta["model"], DEPLOYMENT);
    assert_eq!(meta["provider"], "foundry");
    assert_eq!(meta["cost"]["priced"], true);
    assert_eq!(meta["results"], 2);
    assert_eq!(meta["settings"]["documents"], 3);
    assert_eq!(meta["settings"]["top_n"], 2);
    assert_eq!(meta["settings"]["search_units"], 2);
    assert_eq!(meta["timing"]["attempts"], 1);
}

#[tokio::test]
async fn an_unpriced_reranker_is_recorded_at_zero_and_reported_unpriced() {
    let (base, _) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec![], None, false).await;
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(body["x_router"]["cost"]["priced"], false);
    assert_eq!(body["usage"]["cost_usd"].as_f64(), Some(0.0));
    let ledger = common::wait_for_ledger_rows(&*app.db, 1).await;
    assert_eq!(ledger[0].cost_usd, 0.0);
    assert_eq!(ledger[0].call_kind, "rerank");
}

#[tokio::test]
async fn rerank_url_sends_the_request_to_the_configured_endpoint() {
    let (base, capture) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec![], Some(format!("{base}/v2/rerank")), true).await;
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(capture.lock().unwrap().paths, vec!["/v2/rerank".to_string()]);
}

#[tokio::test]
async fn an_upstream_4xx_is_returned_with_its_body_once_and_never_retried_or_billed() {
    let rejection = json!({"error": {"code": "NotARerankDeployment", "message": "this deployment does not rerank"}});
    let (base, capture) = spawn_upstream(StatusCode::BAD_REQUEST, rejection).await;
    let app = test_app(&base, vec![], None, true).await;
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["error"]["code"], "upstream_rejected");
    assert_eq!(body["error"]["upstream_status"], 400);
    assert_eq!(body["error"]["upstream"]["error"]["code"], "NotARerankDeployment");
    assert_eq!(capture.lock().unwrap().paths.len(), 1, "a 4xx is sent exactly once");
    settle().await;
    assert!(CostRepository::list_cost_entries_before(&*app.db, common::FOREVER).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_upstream_5xx_is_a_provider_error_after_one_call() {
    let (base, capture) = spawn_upstream(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "busy"})).await;
    let app = test_app(&base, vec![], None, true).await;
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 502);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["error"]["code"], "provider_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("503") && message.contains(DEPLOYMENT), "{message}");
    assert_eq!(capture.lock().unwrap().paths.len(), 1);
}

#[tokio::test]
async fn a_malformed_request_or_a_missing_model_is_a_400_before_any_call() {
    let (base, capture) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec![], None, true).await;
    for (body, needle) in [
        (json!({"query": "q", "documents": ["a"]}), "`model`"),
        (json!({"model": ALIAS, "documents": ["a"]}), "`query`"),
        (json!({"model": ALIAS, "query": "q", "documents": []}), "`documents`"),
    ] {
        let (name, value) = bearer();
        let resp = app.server.post("/v1/rerank").add_header(name, value).json(&body).await;
        assert_eq!(resp.status_code(), 400, "{body}");
        let text = resp.text();
        assert!(text.contains(needle), "{text}");
    }
    assert!(capture.lock().unwrap().paths.is_empty());
    settle().await;
    assert!(CostRepository::list_cost_entries_before(&*app.db, common::FOREVER).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_deployment_outside_rerank_deployments_is_refused_before_any_call_and_a_listed_bare_name_is_sent_as_itself() {
    let (base, capture) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec!["rerank-only".to_string()], None, true).await;
    let (name, value) = bearer();
    // The alias resolves to rerank-a, which the list does not name.
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 400);
    // The body is JSON, so the quotes around the name are escaped in the raw text: read the message.
    let message = resp.json::<serde_json::Value>()["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("\"rerank-a\"") && message.contains("rerank_deployments"), "{message}");
    assert!(capture.lock().unwrap().paths.is_empty());

    // A bare listed deployment (no alias, no prefix) resolves to itself.
    let mut body = request_body();
    body["model"] = json!("rerank-only");
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let c = capture.lock().unwrap();
    assert_eq!(c.bodies[0]["model"], "rerank-only");
    assert_eq!(resp.json::<serde_json::Value>()["x_router"]["model"], "rerank-only");
}

#[tokio::test]
async fn an_unlisted_bare_model_is_a_400_with_no_upstream_call_and_no_substitution() {
    let (base, capture) = spawn_upstream(StatusCode::OK, results_payload()).await;
    let app = test_app(&base, vec![], None, true).await;
    let mut body = request_body();
    body["model"] = json!("some-chat-model");
    let (name, value) = bearer();
    let resp = app.server.post("/v1/rerank").add_header(name, value).json(&body).await;
    assert_eq!(resp.status_code(), 400);
    let text = resp.text();
    assert!(text.contains("'some-chat-model'") && !text.contains("chat-default"), "{text}");
    assert!(capture.lock().unwrap().paths.is_empty());
}
