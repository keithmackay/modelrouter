//! `POST /v1/decisions` end to end: the real route, the real Foundry decision
//! adapter, and a fake Foundry resource on a loopback port that records what
//! reached it. Nothing here is mocked below the HTTP boundary.
mod common;

use std::sync::{Arc, Mutex};

use axum::{extract::Json, http::HeaderMap, http::StatusCode, http::Uri, response::IntoResponse, routing::post, Router};
use axum_test::TestServer;
use modelrouter::api::app::{build_router_with_decision_registry, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, PricingEntry, ProviderConfig, Settings};
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::prompts::PromptRepository;
use modelrouter::router::experiments::{ExperimentRegistry as LiveExperiments, ExperimentSnapshot};
use modelrouter::extensions::Extensions;
use modelrouter::providers::azure_entra::StaticTokenProvider;
use modelrouter::providers::decision_registry::DecisionRegistry;
use modelrouter::providers::foundry::FoundryDecisionAdapter;
use modelrouter::providers::{embed_registry::EmbeddingRegistry, registry::ProviderRegistry};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::json;
use std::collections::HashMap;

const DEPLOYMENT: &str = "decision-a";
const ALIAS: &str = "decide";

#[derive(Default)]
struct Capture {
    paths: Vec<String>,
    authorization: Vec<Option<String>>,
    bodies: Vec<serde_json::Value>,
}

/// A fake Foundry resource serving the OpenAI-compatible decisions path.
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
    // The resource serves the OpenAI-compatible decisions path and, beside its
    // data planes, the System One route some decision deployments answer on.
    let app = Router::new()
        .route("/openai/v1/decisions", post(handler.clone()))
        .route("/providers/microsoft/v1/systemone", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), capture)
}

fn answer_payload() -> serde_json::Value {
    json!({
        "id": "dec-1",
        "object": "decision",
        "model": DEPLOYMENT,
        "answers": [{"type": "predicate", "name": "refund_requested", "probability": 0.93}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}
    })
}

fn request_body() -> serde_json::Value {
    json!({
        "model": ALIAS,
        "input": "I was charged twice for my order. Please return the extra charge.",
        "questions": [{"type": "predicate", "name": "refund_requested", "instructions": "Does the customer ask for a refund?"}]
    })
}

fn foundry_config(endpoint: &str, decision_deployments: Vec<String>) -> ProviderConfig {
    ProviderConfig {
        foundry_endpoint: Some(endpoint.to_string()),
        timeout_secs: 30,
        decision_deployments,
        ..Default::default()
    }
}

/// The app with `decide` aliased to `foundry/decision-a`, strict resolution on,
/// a price for the deployment, and the real adapter pointed at `endpoint`.
async fn test_app(endpoint: &str, decision_deployments: Vec<String>) -> (TestServer, Arc<dyn DatabaseProvider>) {
    test_app_with(endpoint, decision_deployments, true).await
}

/// `test_app` with `routing.strict_model_resolution` as given: the decisions
/// route must behave the same either way, because it never substitutes.
async fn test_app_with(
    endpoint: &str,
    decision_deployments: Vec<String>,
    strict_model_resolution: bool,
) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let (server, db, _, _) = test_app_full(endpoint, decision_deployments, strict_model_resolution).await;
    (server, db)
}

/// Everything a test may need to steer: the server, the database, the live
/// experiment registry (to store a snapshot) and the storage policy swap.
async fn test_app_full(
    endpoint: &str,
    decision_deployments: Vec<String>,
    strict_model_resolution: bool,
) -> (
    TestServer,
    Arc<dyn DatabaseProvider>,
    Arc<LiveExperiments>,
    Arc<arc_swap::ArcSwap<modelrouter::config::schema::StorageConfig>>,
) {
    test_app_full_with_config(foundry_config(endpoint, decision_deployments), strict_model_resolution).await
}

/// `test_app_full` from a provider config the test shaped itself (the System
/// One route's deployments, for one).
async fn test_app_full_with_config(
    config: ProviderConfig,
    strict_model_resolution: bool,
) -> (
    TestServer,
    Arc<dyn DatabaseProvider>,
    Arc<LiveExperiments>,
    Arc<arc_swap::ArcSwap<modelrouter::config::schema::StorageConfig>>,
) {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;

    let mut settings = Settings::default();
    settings.routing.model_aliases.insert(ALIAS.to_string(), format!("foundry/{DEPLOYMENT}"));
    settings.routing.strict_model_resolution = strict_model_resolution;
    // The chat default a substitution would have sent the document to.
    settings.routing.default_provider = "foundry".to_string();
    settings.routing.default_model = "foundry/chat-default".to_string();
    settings.providers.insert("foundry".to_string(), config.clone());
    let settings = Arc::new(settings);

    let registry = Arc::new(DecisionRegistry::new(settings.providers.clone()));
    registry.register(
        "foundry",
        FoundryDecisionAdapter::with_token_provider(&config, Arc::new(StaticTokenProvider::new("static-token"))).unwrap(),
    );

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&[PricingEntry {
            model: DEPLOYMENT.to_string(),
            input_per_million: 1000.0,
            output_per_million: 2000.0,
            ..Default::default()
        }])),
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
    let experiments = state.experiments.clone();
    let storage = state.storage.clone();
    let app = build_router_with_decision_registry(state, Extensions::default(), registry);
    (TestServer::new(app).unwrap(), db, experiments, storage)
}

/// Experiment 7: `decide` pinned to `foundry/decision-a` (control) or
/// `foundry/decision-b` (candidate), as the creation gate would store it.
fn two_arm_decision_experiment(retain_content: bool) -> ExperimentSnapshot {
    let overlay = |deployment: &str| {
        let mut m = std::collections::BTreeMap::new();
        m.insert(ALIAS.to_string(), format!("foundry/{deployment}"));
        Arc::new(m)
    };
    let mut overlays = std::collections::BTreeMap::new();
    overlays.insert("candidate".to_string(), overlay("decision-b"));
    overlays.insert("control".to_string(), overlay(DEPLOYMENT));
    ExperimentSnapshot {
        status: modelrouter::db::models::ExperimentStatus::Active,
        expires_at: 0,
        allowed_user_ids: vec![],
        labels: vec!["candidate".to_string(), "control".to_string()],
        overlays,
        retain_content,
    }
}

fn experiment_header(value: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::HeaderName::from_static("x-modelrouter-experiment"),
        axum::http::HeaderValue::from_str(value).unwrap(),
    )
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
async fn decisions_unauthenticated_returns_401_and_sends_nothing() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, _) = test_app(&base, vec![]).await;
    let resp = server.post("/v1/decisions").json(&request_body()).await;
    assert_eq!(resp.status_code(), 401);
    assert!(capture.lock().unwrap().paths.is_empty());
}

#[tokio::test]
async fn a_decision_passes_through_the_body_and_the_answers_and_prices_the_call() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, db) = test_app(&base, vec![]).await;
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();

    // The provider's answer, as it arrived.
    assert_eq!(body["answers"][0]["name"], "refund_requested");
    assert_eq!(body["answers"][0]["probability"].as_f64(), Some(0.93));
    assert_eq!(body["id"], "dec-1");

    // What reached the provider: the caller's body with the resolved deployment.
    {
        let c = capture.lock().unwrap();
        assert_eq!(c.paths, vec!["/openai/v1/decisions".to_string()]);
        assert_eq!(c.authorization[0].as_deref(), Some("Bearer static-token"));
        assert_eq!(c.bodies[0]["model"], DEPLOYMENT);
        assert_eq!(c.bodies[0]["input"], request_body()["input"]);
        assert_eq!(c.bodies[0]["questions"], request_body()["questions"]);
    }

    // Priced from usage like chat input/output, written to the ledger, and
    // reported verbatim in usage and x_router.
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger[0].model, DEPLOYMENT);
    assert_eq!(ledger[0].provider, "foundry");
    assert_eq!(ledger[0].tokens_in, 12);
    assert_eq!(ledger[0].tokens_out, 3);
    let expected = 12.0 * 1000.0 / 1_000_000.0 + 3.0 * 2000.0 / 1_000_000.0;
    assert!((ledger[0].cost_usd - expected).abs() < 1e-9, "ledger cost {}", ledger[0].cost_usd);
    assert_eq!(body["usage"]["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
    assert_eq!(body["usage"]["prompt_tokens"], 12);
    let meta = &body["x_router"];
    assert_eq!(meta["requested_model"], ALIAS);
    assert_eq!(meta["model"], DEPLOYMENT);
    assert_eq!(meta["provider"], "foundry");
    assert_eq!(meta["cost"]["priced"], true);
    assert_eq!(meta["tokens"]["prompt"], 12);
    assert_eq!(meta["tokens"]["completion"], 3);
    assert_eq!(meta["timing"]["attempts"], 1);
    assert_eq!(meta["settings"]["questions"], 1);
}

#[tokio::test]
async fn an_upstream_4xx_is_returned_with_its_body_once_and_never_retried_or_billed() {
    let rejection = json!({"error": {"code": "DeploymentNotDecisionCapable", "message": "this deployment does not support decisions"}});
    let (base, capture) = spawn_upstream(StatusCode::BAD_REQUEST, rejection).await;
    let (server, db) = test_app(&base, vec![]).await;
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["error"]["code"], "upstream_rejected");
    assert_eq!(body["error"]["upstream_status"], 400);
    assert_eq!(body["error"]["upstream"]["error"]["code"], "DeploymentNotDecisionCapable");
    assert_eq!(capture.lock().unwrap().paths.len(), 1, "a 4xx is sent exactly once");
    settle().await;
    assert!(CostRepository::list_cost_entries_before(&*db, common::FOREVER).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_upstream_5xx_is_a_provider_error_after_one_call() {
    let (base, capture) = spawn_upstream(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "busy"})).await;
    let (server, _) = test_app(&base, vec![]).await;
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 502);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["error"]["code"], "provider_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("503") && message.contains(DEPLOYMENT), "{message}");
    assert_eq!(capture.lock().unwrap().paths.len(), 1);
}

#[tokio::test]
async fn a_request_without_a_model_is_a_400_before_any_call() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, _) = test_app(&base, vec![]).await;
    let (name, value) = bearer();
    let mut body = request_body();
    body.as_object_mut().unwrap().remove("model");
    let resp = server.post("/v1/decisions").add_header(name, value).json(&body).await;
    assert_eq!(resp.status_code(), 400);
    assert!(resp.text().contains("`model` is required"), "{}", resp.text());
    assert!(capture.lock().unwrap().paths.is_empty());
}

#[tokio::test]
async fn an_unknown_model_or_a_provider_without_decisions_is_a_clear_400() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, _) = test_app(&base, vec![]).await;

    // Not an alias, no provider prefix: strict resolution refuses the substitution.
    let (name, value) = bearer();
    let mut body = request_body();
    body["model"] = json!("no-such-model");
    let resp = server.post("/v1/decisions").add_header(name, value).json(&body).await;
    assert_eq!(resp.status_code(), 400);
    assert!(resp.text().contains("no-such-model"), "{}", resp.text());

    // A provider that has no decisions surface.
    let (name, value) = bearer();
    body["model"] = json!("openai/gpt-4o-mini");
    let resp = server.post("/v1/decisions").add_header(name, value).json(&body).await;
    assert_eq!(resp.status_code(), 400);
    let text = resp.text();
    assert!(text.contains("openai/gpt-4o-mini") && text.contains("does not serve decisions"), "{text}");

    assert!(capture.lock().unwrap().paths.is_empty());
}

#[tokio::test]
async fn a_deployment_outside_decision_deployments_is_refused_before_any_call() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, _) = test_app(&base, vec!["decision-b".to_string()]).await;
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 400);
    let text = resp.text();
    assert!(text.contains(DEPLOYMENT) && text.contains("decision_deployments"), "{text}");
    assert!(capture.lock().unwrap().paths.is_empty());

    // The named deployment goes through.
    let (server, _) = test_app(&base, vec![DEPLOYMENT.to_string()]).await;
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
}

/// A deployment whose only capability is decisions is not in the chat
/// catalog and has no alias; callers name it bare. It resolves through
/// `decision_deployments` and is sent to the provider as itself.
#[tokio::test]
async fn a_bare_decision_only_deployment_in_decision_deployments_is_sent_as_itself() {
    const DECISION_ONLY: &str = "decision-only-7";
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    for strict in [false, true] {
        let (server, _) = test_app_with(&base, vec![DEPLOYMENT.to_string(), DECISION_ONLY.to_string()], strict).await;
        let (name, value) = bearer();
        let mut body = request_body();
        body["model"] = json!(DECISION_ONLY);
        let resp = server.post("/v1/decisions").add_header(name, value).json(&body).await;
        assert_eq!(resp.status_code(), 200, "strict={strict}: {}", resp.text());
        let out: serde_json::Value = resp.json();
        assert_eq!(out["x_router"]["requested_model"], DECISION_ONLY);
        assert_eq!(out["x_router"]["model"], DECISION_ONLY);
        assert_eq!(out["x_router"]["provider"], "foundry");
    }
    let c = capture.lock().unwrap();
    assert_eq!(c.paths.len(), 2);
    for sent in &c.bodies {
        assert_eq!(sent["model"], DECISION_ONLY, "the deployment the caller named is the one on the wire");
    }
}

/// A bare name no alias knows and no provider lists is refused by name before
/// any call, and is never swapped for `routing.default_model`, whatever
/// `strict_model_resolution` says (chat's substitution rule does not apply).
#[tokio::test]
async fn an_unlisted_bare_model_is_a_400_with_no_upstream_call_and_no_substitution() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    for strict in [false, true] {
        let (server, db) = test_app_with(&base, vec![DEPLOYMENT.to_string()], strict).await;
        let (name, value) = bearer();
        let mut body = request_body();
        body["model"] = json!("gpt-6-nova");
        let resp = server.post("/v1/decisions").add_header(name, value).json(&body).await;
        assert_eq!(resp.status_code(), 400, "strict={strict}: {}", resp.text());
        let out: serde_json::Value = resp.json();
        assert_eq!(out["error"]["type"], "invalid_request", "strict={strict}");
        let message = out["error"]["message"].as_str().unwrap_or_default().to_string();
        assert!(message.contains("'gpt-6-nova'"), "strict={strict}: {message}");
        assert!(message.contains("decision_deployments"), "strict={strict}: {message}");
        assert!(!message.contains("chat-default"), "strict={strict}: the default model is not offered: {message}");
        settle().await;
        assert!(CostRepository::list_cost_entries_before(&*db, common::FOREVER).await.unwrap().is_empty(), "strict={strict}: nothing was billed");
    }
    assert!(capture.lock().unwrap().paths.is_empty(), "no request reached the provider");
}

/// A decision binds to an experiment variant exactly as chat does: the
/// overlay pins the requested model, the rows carry the variant, and every
/// row is stamped `call_kind = decision`.
/// A deployment listed under `systemone_deployments` is answered on the
/// resource's System One route: the request is re-shaped for it (questions
/// keyed by name, the document as `state`), the keyed answers come back as
/// the ordered `answers[]` with their names, and the call is priced from the
/// route's `usage` like any decision. The deployment needs no second listing
/// under `decision_deployments`.
#[tokio::test]
async fn a_systemone_deployment_is_answered_on_the_resources_systemone_route_in_the_decisions_shape() {
    let upstream_answer = json!({
        "model": DEPLOYMENT,
        "answers": {
            "refund_requested": {"type": "predicate", "probability": 0.93},
            "tone": {"type": "choice", "choice": "calm", "probabilities": {"calm": 0.8, "angry": 0.2}, "confidence": 0.8}
        },
        "usage": {"input_tokens": 12, "output_tokens": 3}
    });
    let (base, capture) = spawn_upstream(StatusCode::OK, upstream_answer).await;
    let config = ProviderConfig {
        systemone_deployments: vec![DEPLOYMENT.to_string()],
        ..foundry_config(&base, vec!["another-deployment".to_string()])
    };
    let (server, db, _, _) = test_app_full_with_config(config, true).await;
    let mut request = request_body();
    request["questions"].as_array_mut().unwrap().push(json!({
        "type": "choice", "name": "tone", "instructions": "What is the tone?", "criteria": {"calm": "calm", "angry": "angry"}
    }));
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = resp.json();

    // The caller sees the Decisions shape: answers in question order, each with its name.
    let answers = body["answers"].as_array().expect("answers array");
    assert_eq!(answers.iter().map(|a| a["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["refund_requested", "tone"]);
    assert_eq!(answers[0]["probability"].as_f64(), Some(0.93));
    assert_eq!(answers[1]["choice"], "calm");

    // What reached the resource: the System One route, with the re-shaped body.
    {
        let c = capture.lock().unwrap();
        assert_eq!(c.paths, vec!["/providers/microsoft/v1/systemone".to_string()]);
        assert_eq!(c.authorization[0].as_deref(), Some("Bearer static-token"));
        let sent = &c.bodies[0];
        assert_eq!(sent["model"], DEPLOYMENT);
        assert_eq!(sent["state"], request_body()["input"]);
        assert!(sent.get("input").is_none(), "input travels as state: {sent}");
        assert_eq!(sent["questions"]["refund_requested"]["type"], "predicate");
        assert_eq!(sent["questions"]["refund_requested"]["instructions"], "Does the customer ask for a refund?");
        assert!(sent["questions"]["refund_requested"].get("name").is_none(), "the key carries the name: {sent}");
        assert_eq!(sent["questions"]["tone"]["criteria"]["angry"], "angry");
    }

    // Priced from the route's usage like any decision, and reported as such.
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger[0].model, DEPLOYMENT);
    assert_eq!(ledger[0].tokens_in, 12);
    assert_eq!(ledger[0].tokens_out, 3);
    assert_eq!(body["x_router"]["tokens"]["prompt"], 12);
    assert_eq!(body["x_router"]["settings"]["questions"], 2);
}

/// The route's refusal of a request it does not take comes back as the 4xx it
/// was, with its body, once; and a deployment listed on neither list is still
/// refused before any call when a list is set.
#[tokio::test]
async fn the_systemone_routes_refusal_passes_through_and_unlisted_deployments_are_still_refused_first() {
    let (base, capture) = spawn_upstream(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": {"message": "unknown question type"}})).await;
    let config = ProviderConfig {
        systemone_deployments: vec![DEPLOYMENT.to_string()],
        ..foundry_config(&base, vec![])
    };
    let (server, _db, _, _) = test_app_full_with_config(config, true).await;
    let (name, value) = bearer();
    let resp = server.post("/v1/decisions").add_header(name.clone(), value.clone()).json(&request_body()).await;
    assert_eq!(resp.status_code(), 422, "{}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(body["error"]["code"], "upstream_rejected");
    assert_eq!(body["error"]["upstream"]["error"]["message"], "unknown question type");
    assert_eq!(capture.lock().unwrap().paths, vec!["/providers/microsoft/v1/systemone".to_string()]);

    let mut other = request_body();
    other["model"] = json!("foundry/not-listed");
    let refused = server.post("/v1/decisions").add_header(name, value).json(&other).await;
    assert_eq!(refused.status_code(), 400, "{}", refused.text());
    assert!(refused.text().contains("systemone_deployments"), "{}", refused.text());
    assert_eq!(capture.lock().unwrap().paths.len(), 1, "no second upstream call");
}

#[tokio::test]
async fn a_decision_binds_to_an_experiment_variant_and_its_rows_carry_the_variant_and_kind() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, db, experiments, _) =
        test_app_full(&base, vec![DEPLOYMENT.to_string(), "decision-b".to_string()], true).await;
    experiments.store(std::collections::HashMap::from([(7, two_arm_decision_experiment(false))]));

    let (name, value) = bearer();
    let (hk, hv) = experiment_header("7:candidate");
    let mut body = request_body();
    body["attribution"] = json!({"correlation_id": "run-1"});
    let resp = server.post("/v1/decisions").add_header(name, value).add_header(hk, hv).json(&body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let out: serde_json::Value = resp.json();
    assert_eq!(out["x_router"]["requested_model"], ALIAS);
    assert_eq!(out["x_router"]["model"], "decision-b", "the candidate overlay pinned the deployment");
    assert_eq!(out["x_router"]["settings"]["experiment_id"], 7);
    assert_eq!(out["x_router"]["settings"]["experiment_variant"], "candidate");
    assert_eq!(capture.lock().unwrap().bodies[0]["model"], "decision-b");

    settle().await;
    let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].experiment_id, Some(7));
    assert_eq!(prompts[0].experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(prompts[0].call_kind, "decision");
    assert_eq!(prompts[0].request_model, ALIAS, "the caller's name, as on chat; routed_model is the pin");
    assert_eq!(prompts[0].routed_model, "decision-b");
    let ledger = CostRepository::list_cost_entries_before(&*db, common::FOREVER).await.unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].experiment_id, Some(7));
    assert_eq!(ledger[0].experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(ledger[0].call_kind, "decision");
}

/// A header naming an experiment the router does not know is the caller's
/// 400 before anything is sent, as on every bound route.
#[tokio::test]
async fn an_unknown_experiment_in_the_header_is_a_400_before_any_call() {
    let (base, capture) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, _) = test_app(&base, vec![]).await;
    let (name, value) = bearer();
    let (hk, hv) = experiment_header("1");
    let resp = server.post("/v1/decisions").add_header(name, value).add_header(hk, hv).json(&request_body()).await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    assert!(capture.lock().unwrap().paths.is_empty());
}

/// The prompt log keeps the request (input, questions) and the answers, under
/// the storage policy: metadata only by default, content when the policy
/// stores it or a retaining experiment forces it.
#[tokio::test]
async fn the_prompt_log_keeps_the_decision_request_and_answers_under_the_storage_policy() {
    let (base, _) = spawn_upstream(StatusCode::OK, answer_payload()).await;
    let (server, db, experiments, storage) =
        test_app_full(&base, vec![DEPLOYMENT.to_string(), "decision-b".to_string()], true).await;
    let (name, value) = bearer();

    // Default policy: the row exists, its content is the redaction marker.
    let resp = server.post("/v1/decisions").add_header(name.clone(), value.clone()).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    settle().await;
    let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].messages, modelrouter::db::prompt_store::CONTENT_NOT_STORED);
    assert!(prompts[0].response.is_none());
    assert_eq!(prompts[0].call_kind, "decision");

    // A retaining experiment forces the content on, the chat rule.
    experiments.store(std::collections::HashMap::from([(7, two_arm_decision_experiment(true))]));
    let (hk, hv) = experiment_header("7:control");
    let mut body = request_body();
    body["attribution"] = json!({"correlation_id": "run-2"});
    let resp = server.post("/v1/decisions").add_header(name.clone(), value.clone()).add_header(hk, hv).json(&body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    settle().await;
    let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
    let bound = prompts.iter().find(|p| p.experiment_id == Some(7)).expect("the bound row");
    let request: serde_json::Value = serde_json::from_str(&bound.messages).unwrap();
    assert_eq!(request["questions"][0]["name"], "refund_requested");
    assert!(request["input"].as_str().unwrap().contains("charged twice"));
    let answers: serde_json::Value = serde_json::from_str(bound.response.as_deref().unwrap()).unwrap();
    assert_eq!(answers["answers"][0]["name"], "refund_requested");
    assert_eq!(answers["answers"][0]["probability"], 0.93);

    // A policy that stores content keeps it for an unbound call too.
    storage.store(Arc::new(modelrouter::config::schema::StorageConfig {
        store_prompt_content: true,
        ..Default::default()
    }));
    let resp = server.post("/v1/decisions").add_header(name, value).json(&request_body()).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    settle().await;
    let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
    let unbound_with_content = prompts.iter().filter(|p| p.experiment_id.is_none() && p.response.is_some()).count();
    assert_eq!(unbound_with_content, 1);
}
