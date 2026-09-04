//! `GET /admin/api/compare`: two arms of an experiment side by side.
//!
//! Arms are formed client-side (a model per arm, a tag per request); the
//! endpoint partitions what was recorded. Cost and tokens come from the ledger,
//! latency from `prompts`, failures from `request_failures`.

mod common;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, PricingEntry, Settings};
use modelrouter::db::models::{
    FailureStage, NewApiKey, NewCostLedgerEntry, NewPrompt, NewRequestFailure, NewUser,
};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::failures::FailureRepository;
use modelrouter::db::repositories::prompts::PromptRepository;
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

async fn build_app() -> (TestServer, Arc<dyn DatabaseProvider>, Arc<Settings>) {
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
    let user = UserRepository::find_by_name(&db, "test-user")
        .await
        .unwrap()
        .unwrap();
    ApiKeyRepository::create_api_key(
        &db,
        NewApiKey {
            user_id: user.id,
            key_hash: hash_token("test-token"),
            label: Some("test".to_string()),
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();

    // Two priced models so both arms of a model experiment have a cost; any
    // other model name is unpriced on purpose.
    let settings = Settings {
        pricing: vec![
            PricingEntry {
                model: "mock-model".to_string(),
                input_per_million: 1000.0,
                output_per_million: 1000.0,
                ..Default::default()
            },
            PricingEntry {
                model: "mock-model-b".to_string(),
                input_per_million: 2000.0,
                output_per_million: 2000.0,
                ..Default::default()
            },
        ],
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
    };
    (
        TestServer::new(build_router(state)).unwrap(),
        db,
        settings,
    )
}

fn bearer() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer test-token"),
    )
}

fn admin_jwt(settings: &Settings, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    issue_jwt(
        &AdminClaims {
            sub: 1,
            name: format!("{}-user", role),
            role: role.to_string(),
            exp,
        },
        &settings.auth.jwt_secret,
    )
    .unwrap()
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

/// Drive `n` completions through the proxy on `model`, tagged `arm=<arm>`.
/// Model names carry the `provider/` prefix so they resolve as requested
/// rather than falling through to `routing.default_model`.
async fn drive(server: &TestServer, model: &str, arm: &str, n: usize) {
    for i in 0..n {
        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": format!("q{}", i)}],
                "attribution": {
                    "correlation_id": format!("run-{}", arm),
                    "tags": { "experiment": "exp-1", "arm": arm }
                },
            }))
            .await;
        assert_eq!(resp.status_code(), 200);
    }
}

/// One recorded row per source for a hand-built arm.
struct Seed<'a> {
    model: &'a str,
    provider: &'a str,
    run: &'a str,
    tags: &'a str,
}

async fn seed_ledger(db: &Arc<dyn DatabaseProvider>, s: &Seed<'_>, tokens: (i64, i64), cost: f64) {
    CostRepository::create(
        &**db,
        NewCostLedgerEntry {
            user_id: 1,
            prompt_id: None,
            model: s.model.to_string(),
            provider: s.provider.to_string(),
            project: None,
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            cost_usd: cost,
            api_key_id: None,
            attribution_correlation_id: Some(s.run.to_string()),
            attribution_tags: s.tags.to_string(),
        },
    )
    .await
    .unwrap();
}

async fn seed_cache_hit(db: &Arc<dyn DatabaseProvider>, s: &Seed<'_>, tokens: (i64, i64)) {
    CostRepository::create_cache_hit(
        &**db,
        NewCostLedgerEntry {
            user_id: 1,
            prompt_id: None,
            model: s.model.to_string(),
            provider: s.provider.to_string(),
            project: None,
            tokens_in: tokens.0,
            tokens_out: tokens.1,
            cost_usd: 0.0,
            api_key_id: None,
            attribution_correlation_id: Some(s.run.to_string()),
            attribution_tags: s.tags.to_string(),
        },
    )
    .await
    .unwrap();
}

async fn seed_prompt(db: &Arc<dyn DatabaseProvider>, s: &Seed<'_>, latency_ms: Option<i64>) {
    PromptRepository::create(
        &**db,
        NewPrompt {
            user_id: 1,
            session_id: None,
            request_model: s.model.to_string(),
            routed_model: s.model.to_string(),
            provider: s.provider.to_string(),
            messages: "[]".to_string(),
            response: None,
            finish_reason: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.0,
            latency_ms,
            tags: "[]".to_string(),
            project: None,
            attribution_correlation_id: Some(s.run.to_string()),
            attribution_tags: s.tags.to_string(),
        },
    )
    .await
    .unwrap();
}

async fn seed_failure(db: &Arc<dyn DatabaseProvider>, s: &Seed<'_>) {
    FailureRepository::create(
        &**db,
        NewRequestFailure {
            user_id: Some(1),
            api_key_id: None,
            endpoint: "/v1/chat/completions".to_string(),
            request_model: s.model.to_string(),
            routed_model: Some(s.model.to_string()),
            provider: Some(s.provider.to_string()),
            stage: FailureStage::Provider,
            status_code: Some(502),
            error_message: "upstream".to_string(),
            attempts: 1,
            latency_ms: None,
            project: None,
            attribution_correlation_id: Some(s.run.to_string()),
            attribution_tags: s.tags.to_string(),
        },
    )
    .await
    .unwrap();
}

async fn compare(server: &TestServer, settings: &Settings, query: &str) -> (u16, Value) {
    let resp = server
        .get("/admin/api/compare")
        .add_raw_query_param(query)
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {}", admin_jwt(settings, "admin")))
                .unwrap(),
        )
        .await;
    (resp.status_code().as_u16(), resp.json::<Value>())
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

const P1: Seed<'static> = Seed { model: "m1", provider: "p1", run: "run-1", tags: r#"{"arm":"x"}"# };
const P2: Seed<'static> = Seed { model: "m2", provider: "p2", run: "run-2", tags: r#"{"arm":"y"}"# };

/// Ledger, prompt and failure rows for two hand-built arms with numbers chosen
/// so every derived figure is exact in binary floating point.
async fn seed_two_arms(db: &Arc<dyn DatabaseProvider>) {
    for _ in 0..4 {
        seed_ledger(db, &P1, (10, 20), 0.5).await;
    }
    for ms in [100, 200, 300, 400] {
        seed_prompt(db, &P1, Some(ms)).await;
    }
    seed_failure(db, &P1).await;

    seed_ledger(db, &P2, (40, 80), 0.25).await;
    seed_prompt(db, &P2, Some(400)).await;
}

// ── Happy path through the proxy ──────────────────────────────────────────────

#[tokio::test]
async fn tag_dimension_partitions_traffic_driven_through_the_proxy() {
    let (server, db, settings) = build_app().await;
    drive(&server, "mock/mock-model", "a", 3).await;
    drive(&server, "mock/mock-model-b", "b", 2).await;
    wait_for_ledger_rows(&db, 5).await;

    let (status, body) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    assert_eq!(status, 200, "{}", body);

    assert_eq!(body["dimension"], "tag");
    assert_eq!(body["key"], "arm");
    assert_eq!(body["a"]["label"], "arm=a");
    assert_eq!(body["a"]["requests"], 3);
    assert_eq!(body["a"]["tokens_in"], 30);
    assert_eq!(body["a"]["tokens_out"], 60);
    // mock: 10 in + 20 out at $1000/M each side = $0.03 per request
    assert!((body["a"]["cost_per_request"].as_f64().unwrap() - 0.03).abs() < 1e-9);
    assert_eq!(body["a"]["unpriced"], false);

    assert_eq!(body["b"]["requests"], 2);
    assert!((body["b"]["cost_per_request"].as_f64().unwrap() - 0.06).abs() < 1e-9);
    assert_eq!(body["b"]["unpriced"], false);

    // B minus A: fewer requests, dearer per request.
    assert_eq!(body["delta"]["requests"]["abs"], -1.0);
    assert!(body["delta"]["cost_per_request"]["abs"].as_f64().unwrap() > 0.0);
    assert!((body["delta"]["cost_per_request"]["pct"].as_f64().unwrap() - 100.0).abs() < 1e-6);
}

#[tokio::test]
async fn model_and_run_dimensions_give_the_same_partition_as_the_tag() {
    let (server, db, settings) = build_app().await;
    drive(&server, "mock/mock-model", "a", 3).await;
    drive(&server, "mock/mock-model-b", "b", 2).await;
    wait_for_ledger_rows(&db, 5).await;

    let (_, by_tag) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    let (status, by_model) =
        compare(&server, &settings, "dimension=model&a=mock-model&b=mock-model-b&window=all").await;
    assert_eq!(status, 200, "{}", by_model);
    let (status, by_run) = compare(&server, &settings, "dimension=run&a=run-a&b=run-b&window=all").await;
    assert_eq!(status, 200, "{}", by_run);

    for key in ["requests", "cost_usd", "tokens_in", "tokens_out", "latency", "failures"] {
        assert_eq!(by_model["a"][key], by_tag["a"][key], "model a.{}", key);
        assert_eq!(by_model["b"][key], by_tag["b"][key], "model b.{}", key);
        assert_eq!(by_run["a"][key], by_tag["a"][key], "run a.{}", key);
        assert_eq!(by_run["b"][key], by_tag["b"][key], "run b.{}", key);
    }
    assert_eq!(by_model["a"]["label"], "model=mock-model");
    assert_eq!(by_run["b"]["label"], "correlation_id=run-b");
    assert_eq!(by_model["delta"], by_tag["delta"]);
}

// ── Full document from seeded rows ────────────────────────────────────────────

#[tokio::test]
async fn provider_dimension_matches_the_expected_document() {
    let (server, db, settings) = build_app().await;
    seed_two_arms(&db).await;

    let (status, mut body) = compare(&server, &settings, "dimension=provider&a=p1&b=p2&window=all").await;
    assert_eq!(status, 200, "{}", body);
    // Window bounds are clock-dependent; everything else is fixed.
    assert_eq!(body["start"], "1970-01-01T00:00:00Z");
    assert!(body["end"].as_str().unwrap() > today().as_str());
    body.as_object_mut().unwrap().remove("start");
    body.as_object_mut().unwrap().remove("end");
    let caveats = body.as_object_mut().unwrap().remove("caveats").unwrap();
    assert_eq!(caveats.as_array().unwrap().len(), 2);
    let ttft_note = body.as_object_mut().unwrap().remove("ttft_note").unwrap();
    assert!(ttft_note.as_str().unwrap().contains("not recorded"));

    let day = today();
    let expected = json!({
        "dimension": "provider",
        "key": null,
        "window": "all",
        "a": {
            "value": "p1", "label": "provider=p1",
            "requests": 4,
            "cost_usd": 2.0, "cost_per_request": 0.5, "saved_usd": 0.0,
            "tokens_in": 40, "tokens_out": 80,
            "tokens_in_per_request": 10.0, "tokens_out_per_request": 20.0,
            "cache_hits": 0, "hit_rate": 0.0,
            "failures": 1, "error_rate": 0.2,
            "latency": { "samples": 4, "mean_ms": 250.0, "p50_ms": 200, "p95_ms": 400 },
            "unpriced": true, "unpriced_models": ["m1"],
            "by_day": [{ "key": day, "cost_usd": 2.0, "saved_usd": 0.0, "tokens_in": 40,
                         "tokens_out": 80, "requests": 4, "cache_hits": 0 }]
        },
        "b": {
            "value": "p2", "label": "provider=p2",
            "requests": 1,
            "cost_usd": 0.25, "cost_per_request": 0.25, "saved_usd": 0.0,
            "tokens_in": 40, "tokens_out": 80,
            "tokens_in_per_request": 40.0, "tokens_out_per_request": 80.0,
            "cache_hits": 0, "hit_rate": 0.0,
            "failures": 0, "error_rate": 0.0,
            "latency": { "samples": 1, "mean_ms": 400.0, "p50_ms": 400, "p95_ms": 400 },
            "unpriced": true, "unpriced_models": ["m2"],
            "by_day": [{ "key": day, "cost_usd": 0.25, "saved_usd": 0.0, "tokens_in": 40,
                         "tokens_out": 80, "requests": 1, "cache_hits": 0 }]
        },
        "delta": {
            "requests": { "abs": -3.0, "pct": -75.0 },
            "cost_usd": { "abs": -1.75, "pct": -87.5 },
            "cost_per_request": { "abs": -0.25, "pct": -50.0 },
            "tokens_in": { "abs": 0.0, "pct": 0.0 },
            "tokens_out": { "abs": 0.0, "pct": 0.0 },
            "tokens_in_per_request": { "abs": 30.0, "pct": 300.0 },
            "tokens_out_per_request": { "abs": 60.0, "pct": 300.0 },
            "hit_rate": { "abs": 0.0, "pct": null },
            "error_rate": { "abs": -0.2, "pct": -100.0 },
            "mean_ms": { "abs": 150.0, "pct": 60.0 },
            "p50_ms": { "abs": 200.0, "pct": 100.0 },
            "p95_ms": { "abs": 0.0, "pct": 0.0 }
        },
        "coverage": {
            "a": { "requests": 4, "latency_samples": 4 },
            "b": { "requests": 1, "latency_samples": 1 },
            "incomplete_pairs": null
        },
        "ttft": null
    });
    assert_eq!(body, expected);
}

#[tokio::test]
async fn latency_and_failures_come_from_seeded_rows_per_arm() {
    let (server, db, settings) = build_app().await;
    let a = Seed { model: "m", provider: "p", run: "r-a", tags: r#"{"arm":"a"}"# };
    let b = Seed { model: "m", provider: "p", run: "r-b", tags: r#"{"arm":"b"}"# };
    for ms in [100, 200, 300, 400, 1000] {
        seed_prompt(&db, &a, Some(ms)).await;
        seed_ledger(&db, &a, (1, 1), 0.0).await;
    }
    // A cache hit (0) and a row without latency are not samples.
    seed_prompt(&db, &a, Some(0)).await;
    seed_prompt(&db, &a, None).await;
    seed_failure(&db, &a).await;
    seed_failure(&db, &a).await;
    seed_ledger(&db, &b, (1, 1), 0.0).await;
    seed_prompt(&db, &b, Some(50)).await;

    let (status, body) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["a"]["latency"], json!({ "samples": 5, "mean_ms": 400.0, "p50_ms": 300, "p95_ms": 1000 }));
    assert_eq!(body["a"]["failures"], 2);
    // 2 failures over 5 ledger requests + 2 failures
    assert!((body["a"]["error_rate"].as_f64().unwrap() - 2.0 / 7.0).abs() < 1e-12);
    assert_eq!(body["b"]["failures"], 0);
    assert_eq!(body["b"]["error_rate"], 0.0);
    assert_eq!(body["b"]["latency"]["samples"], 1);
    assert_eq!(body["coverage"]["a"], json!({ "requests": 5, "latency_samples": 5 }));
}

#[tokio::test]
async fn caveats_and_ttft_note_are_on_every_response() {
    let (server, _db, settings) = build_app().await;
    let (status, body) = compare(&server, &settings, "dimension=model&a=x&b=y").await;
    assert_eq!(status, 200, "{}", body);
    let caveats: Vec<&str> = body["caveats"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    assert_eq!(caveats.len(), 2);
    assert!(caveats[0].to_lowercase().contains("quality"), "{:?}", caveats);
    assert!(caveats[1].to_lowercase().contains("stream"), "{:?}", caveats);
    assert!(body["ttft"].is_null());
    assert!(body["ttft_note"].as_str().unwrap().contains("not recorded"));
    // Default window is monthly, like the attribution endpoint.
    assert_eq!(body["window"], "monthly");
}

// ── Edges ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn zero_row_arm_reports_zeros_absent_percentiles_and_no_division() {
    let (server, db, settings) = build_app().await;
    seed_two_arms(&db).await;

    let (status, body) = compare(&server, &settings, "dimension=provider&a=nobody&b=p2&window=all").await;
    assert_eq!(status, 200, "{}", body);
    let a = &body["a"];
    assert_eq!(a["requests"], 0);
    assert_eq!(a["cost_usd"], 0.0);
    assert!(a["cost_per_request"].is_null());
    assert!(a["tokens_in_per_request"].is_null());
    assert_eq!(a["latency"], json!({ "samples": 0, "mean_ms": null, "p50_ms": null, "p95_ms": null }));
    assert_eq!(a["error_rate"], 0.0);
    assert_eq!(a["unpriced"], false);
    assert_eq!(a["by_day"], json!([]));
    assert_eq!(body["coverage"]["a"], json!({ "requests": 0, "latency_samples": 0 }));
    // A is zero: absolute deltas exist, percentages do not.
    assert_eq!(body["delta"]["requests"], json!({ "abs": 1.0, "pct": null }));
    assert!(body["delta"]["cost_per_request"].is_null());
    assert!(body["delta"]["p95_ms"].is_null());
}

#[tokio::test]
async fn unpriced_model_badges_only_its_own_arm() {
    let (server, db, settings) = build_app().await;
    drive(&server, "mock/mock-model", "a", 1).await;
    drive(&server, "mock/no-price-model", "b", 1).await;
    wait_for_ledger_rows(&db, 2).await;

    let (status, body) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["a"]["unpriced"], false);
    assert_eq!(body["a"]["unpriced_models"], json!([]));
    assert_eq!(body["b"]["unpriced"], true);
    assert_eq!(body["b"]["unpriced_models"], json!(["no-price-model"]));
    assert_eq!(body["b"]["cost_usd"], 0.0);
}

#[tokio::test]
async fn rows_recorded_before_a_price_existed_are_flagged_unpriced() {
    // mock-model is priced now, but this row carries tokens and zero spend:
    // it was written when no price existed, so the arm's cost is incomplete.
    let (server, db, settings) = build_app().await;
    let a = Seed { model: "mock-model", provider: "mock", run: "run-a", tags: r#"{"arm":"a"}"# };
    let b = Seed { model: "mock-model", provider: "mock", run: "run-b", tags: r#"{"arm":"b"}"# };
    seed_ledger(&db, &a, (10, 20), 0.0).await;
    seed_ledger(&db, &b, (10, 20), 0.03).await;

    let (status, body) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["a"]["unpriced"], true);
    assert_eq!(body["a"]["unpriced_models"], json!(["mock-model"]));
    assert_eq!(body["b"]["unpriced"], false);
}

#[tokio::test]
async fn cache_hits_at_zero_cost_are_not_unpriced() {
    // A cache hit is usage without spend by design; it must not trip the
    // historical-unpriced trigger.
    let (server, db, settings) = build_app().await;
    let a = Seed { model: "mock-model", provider: "mock", run: "run-a", tags: r#"{"arm":"a"}"# };
    let b = Seed { model: "mock-model", provider: "mock", run: "run-b", tags: r#"{"arm":"b"}"# };
    seed_cache_hit(&db, &a, (10, 20)).await;
    seed_ledger(&db, &b, (10, 20), 0.03).await;

    let (status, body) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["a"]["cache_hits"], 1);
    assert_eq!(body["a"]["unpriced"], false);
}

#[tokio::test]
async fn a_failure_raises_only_that_arms_error_rate() {
    let (server, db, settings) = build_app().await;
    drive(&server, "mock/mock-model", "a", 3).await;
    drive(&server, "mock/mock-model", "b", 3).await;
    wait_for_ledger_rows(&db, 6).await;
    seed_failure(&db, &Seed { model: "mock-model", provider: "mock", run: "run-a", tags: r#"{"arm":"a"}"# }).await;

    let (_, body) = compare(&server, &settings, "dimension=tag&key=arm&a=a&b=b&window=all").await;
    assert_eq!(body["a"]["failures"], 1);
    assert_eq!(body["a"]["error_rate"], 0.25);
    assert_eq!(body["b"]["failures"], 0);
    assert_eq!(body["b"]["error_rate"], 0.0);
    assert_eq!(body["delta"]["error_rate"]["abs"], -0.25);
}

// ── Validation and auth ───────────────────────────────────────────────────────

#[tokio::test]
async fn malformed_queries_are_400_and_name_the_field() {
    let (server, _db, settings) = build_app().await;
    let cases = [
        ("dimension=variant&a=x&b=y", "dimension"),
        ("dimension=model&b=y", "a"),
        ("dimension=model&a=x", "b"),
        ("dimension=model&a=x&b=x", "a and b"),
        ("dimension=tag&a=x&b=y", "key"),
        ("dimension=tag&key=bad%20key!&a=x&b=y", "tag key must contain only"),
        ("dimension=model&a=x&b=y&window=hourly", "window"),
        ("a=x&b=y", "dimension"),
    ];
    let long = "x".repeat(257);
    let long_key = "k".repeat(65);
    let long_cases = [
        (format!("dimension=model&a={}&b=y", long), "a must be at most 256"),
        (format!("dimension=run&a=x&b={}", long), "b must be at most 256"),
        (format!("dimension=tag&key={}&a=x&b=y", long_key), "key must be at most 64"),
    ];
    let cases = cases
        .iter()
        .map(|(q, n)| (q.to_string(), *n))
        .chain(long_cases.iter().map(|(q, n)| (q.clone(), *n)));
    for (query, needle) in cases {
        let query = query.as_str();
        let (status, body) = compare(&server, &settings, query).await;
        assert_eq!(status, 400, "{} -> {}", query, body);
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains(needle), "{}: {:?} lacks {:?}", query, msg, needle);
    }
}

#[tokio::test]
async fn requires_an_admin_jwt_and_accepts_viewers() {
    let (server, _db, settings) = build_app().await;
    let resp = server
        .get("/admin/api/compare")
        .add_raw_query_param("dimension=model&a=x&b=y")
        .await;
    assert_eq!(resp.status_code(), 401);

    let resp = server
        .get("/admin/api/compare")
        .add_raw_query_param("dimension=model&a=x&b=y")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {}", admin_jwt(&settings, "viewer")))
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status_code(), 200);
}

