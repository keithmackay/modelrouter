//! `/v1/systemone` against a real HTTP mock of TypeSafe System One, so the
//! route's actual reqwest call, auth injection and status passthrough run.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{extract::State, http::HeaderMap, http::StatusCode, routing::post, Json, Router};
use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{
    CacheConfig, PolicyConditionConfig, PolicyRuleConfig, PricingEntry, ProviderConfig, Settings,
};
use modelrouter::db::models::NewBudgetRule;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};

const TYPESAFE_KEY: &str = "ts-secret-key";

#[derive(Clone, Default)]
struct Upstream {
    /// (authorization header, body) of every request received.
    seen: Arc<Mutex<Vec<(Option<String>, Value)>>>,
    /// Status + body to serve.
    reply: Arc<Mutex<(u16, Value)>>,
}

async fn upstream_handler(
    State(up): State<Upstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    up.seen.lock().unwrap().push((auth, body));
    let (status, reply) = up.reply.lock().unwrap().clone();
    (StatusCode::from_u16(status).unwrap(), Json(reply))
}

fn choice_answer() -> Value {
    json!({
        "model": "jev-1.13",
        "answers": {
            "reason": {
                "type": "choice",
                "choice": "timeout",
                "probabilities": { "timeout": 0.9, "other": 0.1 },
                "confidence": 0.9
            }
        },
        "usage": { "input_tokens": 1000, "output_tokens": 10 }
    })
}

async fn start_upstream(status: u16, reply: Value) -> (String, Upstream) {
    let up = Upstream {
        seen: Arc::default(),
        reply: Arc::new(Mutex::new((status, reply))),
    };
    let app = Router::new()
        .route("/v1/systemone", post(upstream_handler))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{}/v1", addr), up)
}

struct Opts {
    api_base: Option<String>,
    api_key: &'static str,
    pricing: Vec<PricingEntry>,
    policy_rules: Vec<PolicyRuleConfig>,
    concurrency: Arc<modelrouter::router::concurrency::ConcurrencyLimiter>,
}

async fn test_app(opts: Opts) -> (TestServer, Arc<dyn DatabaseProvider>) {
    let (server, db, _user_id) = test_app_with_user_id(opts).await;
    (server, db)
}

async fn test_app_with_user_id(opts: Opts) -> (TestServer, Arc<dyn DatabaseProvider>, i64) {
    let db = common::in_memory_db().await;
    let user_id = common::create_user(&db, "test-user", "test-token").await;

    let mut settings = Settings::default();
    settings.pricing = opts.pricing;
    settings.policy_rules = opts.policy_rules;
    if let Some(api_base) = opts.api_base {
        let provider: ProviderConfig = toml::from_str(&format!(
            "api_key = \"{}\"\napi_base = \"{}\"\ntimeout_secs = 5\n",
            opts.api_key, api_base
        ))
        .unwrap();
        settings.providers.insert("typesafe".to_string(), provider);
    }
    let settings = Arc::new(settings);
    let live_settings = Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone()));
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

    let state = AppState {
        settings: settings.clone(),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "hello".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone()).with_settings(live_settings.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter {
                embedding: vec![0.1_f32],
            },
        )),
        search_registry: Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: vec![],
        })),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            HashMap::new(),
        )),
        concurrency: opts.concurrency.clone(),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
        live_settings,
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
    };
    (TestServer::new(build_router(state)).unwrap(), db, user_id)
}

fn opts(api_base: Option<String>) -> Opts {
    Opts {
        api_base,
        api_key: TYPESAFE_KEY,
        pricing: vec![],
        policy_rules: vec![],
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
    }
}

fn request_body() -> Value {
    json!({
        "state": "Activity task timed out after 300s",
        "model": "jev-latest",
        "questions": {
            "reason": {
                "type": "choice",
                "instructions": "Why did the reviewer stop?",
                "criteria": { "timeout": "timed out", "other": "anything else" }
            }
        }
    })
}

async fn call(server: &TestServer, body: &Value) -> axum_test::TestResponse {
    server
        .post("/v1/systemone")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(body)
        .await
}

#[tokio::test]
async fn unauthenticated_returns_401_without_calling_upstream() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let (server, _) = test_app(opts(Some(base))).await;
    let resp = server.post("/v1/systemone").json(&request_body()).await;
    assert_eq!(resp.status_code(), 401);
    assert!(up.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn forwards_body_verbatim_with_router_held_key_and_meters_cost() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let mut o = opts(Some(base));
    o.pricing = vec![PricingEntry {
        model: "systemone/jev-latest".to_string(),
        input_per_million: 2.0,
        output_per_million: 10.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }];
    let (server, db) = test_app(o).await;

    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.json::<Value>(), choice_answer());

    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0.as_deref(), Some(format!("Bearer {}", TYPESAFE_KEY).as_str()));
    assert_eq!(seen[0].1, request_body());

    let rows = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(rows[0].model, "systemone/jev-latest");
    assert_eq!(rows[0].provider, "typesafe");
    assert_eq!(rows[0].tokens_in, 1000);
    assert_eq!(rows[0].tokens_out, 10);
    // 1000 * 2/1M + 10 * 10/1M
    assert!((rows[0].cost_usd - 0.0021).abs() < 1e-12);
}

#[tokio::test]
async fn rate_limit_and_overload_pass_through_for_client_backoff() {
    for status in [429u16, 529, 422] {
        let (base, _) = start_upstream(status, json!({ "error": "slow down" })).await;
        let (server, _) = test_app(opts(Some(base))).await;
        let resp = call(&server, &request_body()).await;
        assert_eq!(resp.status_code().as_u16(), status);
        assert_eq!(resp.json::<Value>(), json!({ "error": "slow down" }));
    }
}

#[tokio::test]
async fn upstream_auth_rejection_becomes_502_not_caller_401() {
    let (base, _) = start_upstream(401, json!({ "error": "bad key" })).await;
    let (server, _) = test_app(opts(Some(base))).await;
    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code(), 502);
    assert!(resp.text().contains("providers.typesafe"));
}

#[tokio::test]
async fn unconfigured_provider_is_a_named_502() {
    let (server, _) = test_app(opts(None)).await;
    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code(), 502);
    assert!(resp.text().contains("[providers.typesafe]"));
}

#[tokio::test]
async fn empty_api_key_is_a_named_502_without_calling_upstream() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let mut o = opts(Some(base));
    o.api_key = "";
    let (server, _) = test_app(o).await;
    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code(), 502);
    assert!(resp.text().contains("api_key"));
    assert!(up.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_model_or_questions_is_400() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let (server, _) = test_app(opts(Some(base))).await;
    let mut no_model = request_body();
    no_model.as_object_mut().unwrap().remove("model");
    assert_eq!(call(&server, &no_model).await.status_code(), 400);
    let mut no_questions = request_body();
    no_questions["questions"] = json!("nope");
    assert_eq!(call(&server, &no_questions).await.status_code(), 400);
    assert!(up.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn policy_gates_the_systemone_pseudo_model() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let mut o = opts(Some(base));
    o.policy_rules = vec![PolicyRuleConfig {
        name: "no-systemone".to_string(),
        condition: PolicyConditionConfig::default(),
        allow_models: vec!["gpt-4o".to_string()],
        budget_usd: None,
        window: "monthly".to_string(),
        priority: 10,
        cache: None,
    }];
    let (server, _) = test_app(o).await;
    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code(), 403);
    assert!(up.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn attribution_extension_field_is_recorded_but_not_forwarded_upstream() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let mut o = opts(Some(base));
    o.pricing = vec![PricingEntry {
        model: "systemone/jev-latest".to_string(),
        input_per_million: 2.0,
        output_per_million: 10.0,
        cache_read_per_million: None,
        cache_write_per_million: None,
    }];
    let (server, db) = test_app(o).await;

    let mut body = request_body();
    body["attribution"] = json!({ "correlation_id": "eng-4711-run-3", "tags": { "phase": "research" } });
    let resp = call(&server, &body).await;
    assert_eq!(resp.status_code(), 200);

    // The upstream never sees the attribution field, and nothing else about
    // the body changed.
    let seen = up.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, request_body());
    assert!(seen[0].1.get("attribution").is_none());

    // It IS captured in the router's own ledger.
    let rows = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(rows[0].attribution_correlation_id.as_deref(), Some("eng-4711-run-3"));
}

#[tokio::test]
async fn unpriced_model_is_metered_at_zero_cost() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    // No [[pricing]] entry for systemone/jev-latest.
    let (server, db) = test_app(opts(Some(base))).await;

    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(up.seen.lock().unwrap().len(), 1);

    let rows = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(rows[0].model, "systemone/jev-latest");
    assert_eq!(rows[0].tokens_in, 1000);
    assert_eq!(rows[0].tokens_out, 10);
    assert_eq!(rows[0].cost_usd, 0.0);
}

#[tokio::test]
async fn policy_concurrency_limit_is_enforced() {
    let (base, up) = start_upstream(200, choice_answer()).await;
    let concurrency = Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new());
    let mut o = opts(Some(base));
    o.concurrency = concurrency.clone();
    let (server, db, user_id) = test_app_with_user_id(o).await;

    BudgetRepository::create(
        &*db,
        NewBudgetRule {
            user_id: Some(user_id),
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: Some(1),
            model_allow: vec![],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    // Hold the user's one permit on the exact ConcurrencyLimiter instance the
    // route itself uses (same pattern as tests/test_images.rs), so the route
    // must see it as exhausted without any real concurrent HTTP traffic.
    let _held_permit = concurrency.try_acquire(user_id, 1).expect("first acquire should succeed");

    let resp = call(&server, &request_body()).await;
    assert_eq!(resp.status_code().as_u16(), 429);
    assert!(up.seen.lock().unwrap().is_empty());
}
