//! `POST /v1/feedback`: a caller reports a run's outcome under its own key.
//!
//! Outcomes are keyed by `(user, correlation_id)`; a later report replaces the
//! earlier one. The run must already have a ledger or failure row under the
//! caller's user, so one key can neither score nor discover another key's run.

mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, PricingEntry, Settings};
use modelrouter::db::models::{
    FailureStage, NewApiKey, NewCostLedgerEntry, NewRequestFailure, NewUser,
};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::failures::FailureRepository;
use modelrouter::db::repositories::outcomes::OutcomeRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search::SearchResultItem,
    search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const FOREVER: &str = "2999-01-01T00:00:00Z";
/// Bearer token of the first user (id 1).
const TOKEN_A: &str = "token-a";
/// Bearer token of the second user (id 2).
const TOKEN_B: &str = "token-b";

async fn create_user(db: &impl DatabaseProvider, name: &str, token: &str) -> i64 {
    UserRepository::create(
        db,
        NewUser {
            name: name.to_string(),
            email: None,
        },
    )
    .await
    .unwrap();
    let user = UserRepository::find_by_name(db, name).await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(
        db,
        NewApiKey {
            user_id: user.id,
            key_hash: hash_token(token),
            label: Some(name.to_string()),
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();
    user.id
}

/// Two users with one key each, so cross-key scoping can be exercised.
async fn build_app() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;
    assert_eq!(create_user(&db, "user-a", TOKEN_A).await, 1);
    assert_eq!(create_user(&db, "user-b", TOKEN_B).await, 2);

    let settings = Settings {
        pricing: vec![PricingEntry {
            model: "mock-model".to_string(),
            input_per_million: 1000.0,
            output_per_million: 1000.0,
            ..Default::default()
        }],
        ..Default::default()
    };
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&settings.pricing)),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
            response: "hello".to_string(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter {
                embedding: vec![0.1_f32, 0.2],
            },
        )),
        search_registry: Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: vec![SearchResultItem {
                title: "Example Domain".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Example description".to_string(),
                score: Some(0.9),
                published_date: None,
            }],
        })),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(
            modelrouter::router::session_affinity::SessionAffinityMap::new(1800),
        ),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
    )
}

/// Cost logging is fire-and-forget, so poll until the expected number of ledger
/// rows exists rather than sleeping for a guessed interval.
async fn wait_for_ledger_rows(db: &Arc<dyn DatabaseProvider>, want: i64) {
    for _ in 0..200 {
        let rows = CostRepository::list_cost_entries_before(&**db, FOREVER)
            .await
            .unwrap();
        if rows.len() as i64 >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {} cost-ledger rows", want);
}

/// One completion through the proxy under `run`, then wait for its ledger row.
async fn complete(server: &TestServer, db: &Arc<dyn DatabaseProvider>, token: &str, run: &str) {
    let before = CostRepository::list_cost_entries_before(&**db, FOREVER)
        .await
        .unwrap()
        .len() as i64;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(bearer(token).0, bearer(token).1)
        .json(&json!({
            "model": "mock/mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "attribution": { "correlation_id": run },
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    wait_for_ledger_rows(db, before + 1).await;
}

/// A hand-built ledger row, optionally stamped with an experiment.
async fn seed_ledger(
    db: &Arc<dyn DatabaseProvider>,
    user_id: i64,
    run: &str,
    stamp: Option<(i64, &str)>,
) {
    CostRepository::create(
        &**db,
        NewCostLedgerEntry {
            user_id,
            prompt_id: None,
            model: "mock-model".to_string(),
            provider: "mock".to_string(),
            project: None,
            tokens_in: 10,
            tokens_out: 5,
            cost_usd: 0.001,
            api_key_id: None,
            attribution_correlation_id: Some(run.to_string()),
            attribution_tags: "{}".to_string(),
            experiment_id: stamp.map(|(id, _)| id),
            experiment_variant: stamp.map(|(_, v)| v.to_string()),
            tokens_estimated: false,
        },
    )
    .await
    .unwrap();
}

async fn seed_failure(db: &Arc<dyn DatabaseProvider>, user_id: i64, run: &str) {
    FailureRepository::create(
        &**db,
        NewRequestFailure {
            user_id: Some(user_id),
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: "mock-model".to_string(),
            routed_model: Some("mock-model".to_string()),
            provider: Some("mock".to_string()),
            stage: FailureStage::Provider,
            status_code: Some(502),
            error_message: "upstream".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some(run.to_string()),
            attribution_tags: "{}".to_string(),
            experiment_id: None,
            experiment_variant: None,
        },
    )
    .await
    .unwrap();
}

async fn feedback(server: &TestServer, token: &str, body: Value) -> (u16, Value) {
    let resp = server
        .post("/v1/feedback")
        .add_header(bearer(token).0, bearer(token).1)
        .json(&body)
        .await;
    (resp.status_code().as_u16(), resp.json::<Value>())
}

fn error_message(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or("")
}

#[tokio::test]
async fn records_outcome_after_a_completion_and_a_later_report_replaces_it() {
    let (server, db) = build_app().await;
    complete(&server, &db, TOKEN_A, "run-1").await;

    let (status, body) = feedback(
        &server,
        TOKEN_A,
        json!({
            "correlation_id": "run-1",
            "outcome": "success",
            "score": 0.8,
            "rating": 4,
            "note": "first pass",
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["user_id"], 1);
    assert_eq!(body["attribution_correlation_id"], "run-1");
    assert_eq!(body["outcome"], "success");
    assert_eq!(body["score"], 0.8);
    assert_eq!(body["rating"], 4);
    assert_eq!(body["note"], "first pass");
    // The completion was not bound to an experiment.
    assert_eq!(body["experiment_id"], Value::Null);
    assert_eq!(body["experiment_variant"], Value::Null);
    let created_at = body["created_at"].as_str().unwrap().to_string();

    let (status, body) = feedback(
        &server,
        TOKEN_A,
        json!({ "correlation_id": "run-1", "outcome": "failure" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcome"], "failure");
    // Optional fields absent from the later report are cleared, not kept.
    assert_eq!(body["score"], Value::Null);
    assert_eq!(body["rating"], Value::Null);
    assert_eq!(body["note"], Value::Null);
    assert_eq!(body["created_at"], created_at);

    let stored = OutcomeRepository::get(&*db, 1, "run-1").await.unwrap().unwrap();
    assert_eq!(stored.outcome, "failure");
    assert_eq!(stored.score, None);
    assert_eq!(stored.rating, None);
}

#[tokio::test]
async fn outcome_carries_the_stamp_of_the_earliest_stamped_ledger_row() {
    let (server, db) = build_app().await;
    seed_ledger(&db, 1, "run-x", None).await;
    seed_ledger(&db, 1, "run-x", Some((7, "b"))).await;
    seed_ledger(&db, 1, "run-x", Some((7, "a"))).await;

    let (status, body) = feedback(
        &server,
        TOKEN_A,
        json!({ "correlation_id": "run-x", "outcome": "success" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["experiment_id"], 7);
    assert_eq!(body["experiment_variant"], "b");
}

#[tokio::test]
async fn a_run_whose_only_row_is_a_failure_is_accepted() {
    let (server, db) = build_app().await;
    seed_failure(&db, 1, "run-failed").await;

    let (status, body) = feedback(
        &server,
        TOKEN_A,
        json!({ "correlation_id": "run-failed", "outcome": "failure", "note": "timed out" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcome"], "failure");
    assert_eq!(body["experiment_id"], Value::Null);
    assert!(OutcomeRepository::get(&*db, 1, "run-failed")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn another_users_run_is_rejected_and_their_outcome_is_untouched() {
    let (server, db) = build_app().await;
    complete(&server, &db, TOKEN_A, "shared-id").await;
    let (status, _) = feedback(
        &server,
        TOKEN_A,
        json!({ "correlation_id": "shared-id", "outcome": "success", "rating": 5 }),
    )
    .await;
    assert_eq!(status, 200);

    // Same correlation id, other key: user B has no run by that name.
    let (status, body) = feedback(
        &server,
        TOKEN_B,
        json!({ "correlation_id": "shared-id", "outcome": "failure" }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        error_message(&body).contains("no recorded requests under this API key"),
        "{body}"
    );
    assert!(error_message(&body).contains("retry"), "{body}");

    let a = OutcomeRepository::get(&*db, 1, "shared-id").await.unwrap().unwrap();
    assert_eq!(a.outcome, "success");
    assert_eq!(a.rating, Some(5));
    assert!(OutcomeRepository::get(&*db, 2, "shared-id")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn unrecorded_run_is_rejected_with_a_retry_hint() {
    let (server, db) = build_app().await;
    let (status, body) = feedback(
        &server,
        TOKEN_A,
        json!({ "correlation_id": "never-ran", "outcome": "success" }),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request");
    let msg = error_message(&body);
    assert!(msg.contains("never-ran"), "{msg}");
    assert!(msg.contains("no recorded requests under this API key yet"), "{msg}");
    assert!(msg.contains("retry"), "{msg}");
    assert!(OutcomeRepository::get(&*db, 1, "never-ran")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn invalid_fields_are_rejected_by_name() {
    let (server, db) = build_app().await;
    complete(&server, &db, TOKEN_A, "run-v").await;

    let cases: Vec<(Value, &str)> = vec![
        (
            json!({ "correlation_id": "run-v", "outcome": "success", "rating": 6 }),
            "rating",
        ),
        (
            json!({ "correlation_id": "run-v", "outcome": "success", "rating": 2.5 }),
            "rating",
        ),
        (
            json!({ "correlation_id": "run-v", "outcome": "success", "score": 1.5 }),
            "score",
        ),
        (
            json!({ "correlation_id": "run-v", "outcome": "success", "score": -0.1 }),
            "score",
        ),
        (json!({ "correlation_id": "run-v", "outcome": "ok" }), "outcome"),
        (json!({ "correlation_id": "run-v" }), "outcome"),
        (json!({ "outcome": "success" }), "correlation_id"),
        (json!({ "correlation_id": "", "outcome": "success" }), "correlation_id"),
        (json!({ "correlation_id": 42, "outcome": "success" }), "correlation_id"),
        (
            json!({ "correlation_id": "x".repeat(129), "outcome": "success" }),
            "correlation_id",
        ),
        (
            json!({ "correlation_id": "a\nb", "outcome": "success" }),
            "correlation_id",
        ),
        (
            json!({ "correlation_id": "run-v", "outcome": "success", "note": "n".repeat(1025) }),
            "note",
        ),
        (
            json!({ "correlation_id": "run-v", "outcome": "success", "note": ["x"] }),
            "note",
        ),
    ];
    for (body, field) in cases {
        let (status, resp) = feedback(&server, TOKEN_A, body.clone()).await;
        assert_eq!(status, 400, "{body} -> {resp}");
        assert_eq!(resp["error"]["type"], "invalid_request", "{body} -> {resp}");
        assert!(
            error_message(&resp).starts_with(field),
            "{body} -> {resp} should name `{field}`"
        );
    }
    // Nothing invalid was stored.
    assert!(OutcomeRepository::get(&*db, 1, "run-v").await.unwrap().is_none());
}

#[tokio::test]
async fn boundary_values_are_accepted() {
    let (server, db) = build_app().await;
    complete(&server, &db, TOKEN_A, "run-b").await;

    for (score, rating) in [(0.0, 1), (1.0, 5)] {
        let (status, body) = feedback(
            &server,
            TOKEN_A,
            json!({
                "correlation_id": "run-b",
                "outcome": "success",
                "score": score,
                "rating": rating,
                "note": "n".repeat(1024),
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["score"], score);
        assert_eq!(body["rating"], rating);
    }
}

#[tokio::test]
async fn requires_an_api_key() {
    let (server, _db) = build_app().await;
    let resp = server
        .post("/v1/feedback")
        .json(&json!({ "correlation_id": "run-1", "outcome": "success" }))
        .await;
    assert_eq!(resp.status_code(), 401);
}
