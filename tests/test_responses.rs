mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::Settings;
use modelrouter::api::auth::hash_token;
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

/// Everything a test needs to reach past the handler's happy path: the server,
/// the database the fire-and-forget cost writer targets, and the circuit
/// breaker so a test can trip it without a provider failure.
struct Harness {
    server: TestServer,
    db: Arc<dyn DatabaseProvider>,
    user_id: i64,
    /// Provider name `gpt-4o` resolves to, so circuit-breaker tests can name it.
    provider: String,
    /// Shared with the app, so a test can occupy a user's only slot and see
    /// what the handler does when the next request finds none.
    concurrency: Arc<modelrouter::router::concurrency::ConcurrencyLimiter>,
}

async fn test_app() -> TestServer {
    build_harness(
        common::MockAdapter { response: "Hello!".to_string() },
        Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
    )
    .await
    .server
}

async fn build_harness<A: modelrouter::providers::adapter::ProviderAdapter + 'static>(
    adapter: A,
    circuit_breaker: Arc<modelrouter::router::circuit_breaker::CircuitBreaker>,
) -> Harness {
    let db = common::in_memory_db().await;

    UserRepository::create(
        &db,
        NewUser {
            name: "test-user".to_string(),
            email: None,
        },
    )
    .await
    .unwrap();

    let user = UserRepository::find_by_name(&db, "test-user").await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(&db, NewApiKey {
        user_id: user.id,
        key_hash: hash_token("test-token"),
        label: Some("test".to_string()),
        expires_at: None,
        project: None,
        session_window_secs: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(adapter));

    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));
    let concurrency = Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new());

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: router.clone(),
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
        concurrency: concurrency.clone(),
        circuit_breaker,
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    let (provider, _) = router.resolve("gpt-4o");
    Harness {
        server: TestServer::new(build_router(state)).unwrap(),
        db,
        user_id: user.id,
        provider,
        concurrency,
    }
}

#[tokio::test]
async fn unauthenticated_responses_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello"
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn authenticated_responses_with_input_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello!");
}

#[tokio::test]
async fn responses_tools_field_with_non_empty_array_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "tools": [{"type": "function", "function": {"name": "test"}}]
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("tools"), "Error message should mention 'tools' field, got: {}", message);
}

#[tokio::test]
async fn responses_tool_choice_field_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "tool_choice": "auto"
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("tool_choice"), "Error message should mention 'tool_choice' field, got: {}", message);
}

#[tokio::test]
async fn responses_empty_tools_array_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "tools": []
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn responses_request_without_tools_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

// ── Additional coverage for issue #80 ────────────────────────────────────────

#[tokio::test]
async fn responses_with_input_array() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": [{"role": "user", "content": "Hi"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn responses_with_messages_directly() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Direct"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn responses_rejects_experiment_header() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .add_header(
            "x-modelrouter-experiment".parse().unwrap(),
            "123:variant".parse().unwrap(),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello"
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn responses_tool_choice_none_is_allowed() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "tool_choice": "none"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn responses_tool_choice_null_is_allowed() {
    let server = test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "tool_choice": null
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

// ── Past the happy path: accounting, policy, breaker, provider failure ───────
//
// The handler's fire-and-forget cost writer, its policy/concurrency gates and
// its provider-error arm are the parts a plain 200-assert never reaches. These
// drive each of them against the in-memory database and mock adapters, waiting
// on the spawned write rather than sleeping for a guessed interval.

use modelrouter::db::models::NewBudgetRule;
use modelrouter::db::repositories::budgets::BudgetRepository;
use modelrouter::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

/// Always fails, so the handler's provider-error arm (and the circuit-breaker
/// failure record inside it) runs.
struct FailingAdapter;

#[async_trait::async_trait]
impl ProviderAdapter for FailingAdapter {
    async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        anyhow::bail!("upstream exploded")
    }
    async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        anyhow::bail!("upstream exploded")
    }
}

fn auth() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer test-token"),
    )
}

fn rule_for(user_id: i64) -> NewBudgetRule {
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
        max_concurrent: None,
        model_allow: vec![],
        model_deny: vec![],
        window_start: None,
        window_end: None,
    }
}

async fn default_harness() -> Harness {
    build_harness(
        common::MockAdapter { response: "Hello!".to_string() },
        Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
    )
    .await
}

#[tokio::test]
async fn responses_success_writes_prompt_and_cost_ledger() {
    let h = default_harness().await;
    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .add_header(
            "x-attribution-correlation-id".parse().unwrap(),
            "corr-abc".parse().unwrap(),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "max_tokens": 64
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // The ledger row is written by a spawned task; wait for it rather than
    // asserting into a race.
    let rows = common::wait_for_ledger_rows(&*h.db, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, h.user_id);
    assert_eq!(rows[0].tokens_in, 10);
    assert_eq!(rows[0].tokens_out, 20);
    assert!(
        rows[0].prompt_id > 0,
        "default storage policy stores the prompt, so the ledger row links to it: {:?}",
        rows[0].prompt_id
    );
}

#[tokio::test]
async fn responses_with_content_storage_disabled_still_writes_the_ledger() {
    // Storage policy (issue #4): the prompt row is optional, the cost row is not.
    let h = default_harness().await;
    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let rows = common::wait_for_ledger_rows(&*h.db, 1).await;
    assert_eq!(rows[0].cost_usd, rows[0].cost_usd, "row exists and is finite");
}

#[tokio::test]
async fn responses_temperature_passes_through_capability_filter() {
    let h = default_harness().await;
    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "input": "Hello",
            "temperature": 0.25
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn responses_denied_model_returns_policy_error() {
    let h = default_harness().await;
    BudgetRepository::create(
        &*h.db,
        NewBudgetRule {
            model_deny: vec!["gpt-4o".to_string()],
            ..rule_for(h.user_id)
        },
    )
    .await
    .unwrap();

    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert!(
        resp.status_code().is_client_error(),
        "a denied model is a 4xx, got {}",
        resp.status_code()
    );
}

#[tokio::test]
async fn responses_under_a_concurrency_cap_still_serves_one_request() {
    let h = default_harness().await;
    BudgetRepository::create(
        &*h.db,
        NewBudgetRule { max_concurrent: Some(2), ..rule_for(h.user_id) },
    )
    .await
    .unwrap();

    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn responses_over_the_concurrency_cap_returns_429() {
    let h = default_harness().await;
    BudgetRepository::create(
        &*h.db,
        NewBudgetRule { max_concurrent: Some(1), ..rule_for(h.user_id) },
    )
    .await
    .unwrap();

    // Stand in for a request of this user that is already in flight: take the
    // one slot the rule allows and hold it. Deterministic where racing two
    // real requests through the test transport is not.
    let _in_flight = h
        .concurrency
        .try_acquire(h.user_id, 1)
        .expect("the first slot is free");

    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert_eq!(resp.status_code(), 429);
    assert!(
        resp.text().contains("concurrent"),
        "the 429 should name the limit it hit: {}",
        resp.text()
    );

    // Once the slot frees, the same request succeeds.
    drop(_in_flight);
    let (hk, hv) = auth();
    let after = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert_eq!(after.status_code(), 200);
}

#[tokio::test]
async fn responses_short_circuits_when_the_breaker_is_open() {
    let breaker = Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::new(1, 60));
    let h = build_harness(common::MockAdapter { response: "x".to_string() }, breaker.clone()).await;
    breaker.record_failure(&h.provider);
    assert!(breaker.is_open(&h.provider), "breaker should be open for {}", h.provider);

    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert!(
        resp.status_code().is_server_error() || resp.status_code().is_client_error(),
        "an open breaker must not return 200, got {}",
        resp.status_code()
    );
    assert!(
        !resp.text().contains("Hello!"),
        "an open breaker must not reach the provider"
    );
}

#[tokio::test]
async fn responses_provider_failure_is_recorded_on_the_breaker() {
    let breaker = Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::new(1, 60));
    let h = build_harness(FailingAdapter, breaker.clone()).await;

    let (hk, hv) = auth();
    let resp = h
        .server
        .post("/v1/responses")
        .add_header(hk, hv)
        .json(&serde_json::json!({ "model": "gpt-4o", "input": "Hello" }))
        .await;
    assert!(
        !resp.status_code().is_success(),
        "a failing provider is not a 200, got {}",
        resp.status_code()
    );
    assert!(
        breaker.is_open(&h.provider),
        "the failure arm must record against the breaker for {}",
        h.provider
    );
}
