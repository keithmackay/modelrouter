//! End-to-end: a mock experiment driven through the real request path, then
//! read back through the results API.
//!
//! Every other experiment test covers one half of the feature. `test_experiments`
//! sends real requests and inspects the rows they wrote; `test_experiments_admin`
//! fabricates rows with `seed::` and inspects the document built from them.
//! Neither crosses the seam, so a mismatch between the columns the request path
//! stamps and the columns the results queries group on would pass both suites
//! while the dashboard showed an empty experiment.
//!
//! This file closes that loop. It creates the experiment over HTTP, drives real
//! completions against a mock provider reached through the production
//! `OpenAICompatAdapter`, reports outcomes over HTTP, and then asserts the
//! results document — with the *mock itself* as the oracle. Expected token and
//! cost figures are summed from what the mock recorded serving, never written
//! as literals, so the assertions describe the router's arithmetic rather than
//! restating it.
//!
//! The scenario is the one an operator would actually run: two models answering
//! the same traffic, where the candidate is terser but slower. Both are priced
//! identically, so any cost difference the API reports is attributable to token
//! behaviour alone.

mod common;

use common::create_user;
use common::mock_llm::{MockLlm, ModelProfile};
use std::collections::HashMap;
use std::sync::Arc;

use axum_test::TestServer;
use modelrouter::api::admin::auth::{issue_jwt, AdminClaims};
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::schema::{CacheConfig, PricingEntry, ProviderConfig, Settings};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::cache::ResponseCache;
use modelrouter::router::experiments::{ExperimentRegistry, EXPERIMENT_HEADER};
use modelrouter::router::{
    complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};

/// Bearer token of the caller running the experiment (user id 1).
const TOKEN: &str = "token-e2e";

/// The alias every request asks for; each variant overlays it onto its model.
const ALIAS: &str = "planner";

/// Priced identically, so the experiment isolates model behaviour from rates.
const INPUT_PER_MILLION: f64 = 1_000.0;
const OUTPUT_PER_MILLION: f64 = 3_000.0;

/// Control's model: chatty and quick.
const CONTROL_MODEL: &str = "model-a";
/// Candidate's model: terser, but slower — the trend under test.
const CANDIDATE_MODEL: &str = "model-b";

/// Runs and their turn counts. Both arms carry the same shape, so a difference
/// in the results is a difference in the models, not in the traffic.
const CONTROL_RUNS: &[(&str, usize)] = &[("c1", 2), ("c2", 1), ("c3", 3), ("c4", 1)];
const CANDIDATE_RUNS: &[(&str, usize)] = &[("k1", 2), ("k2", 1), ("k3", 3), ("k4", 1)];

/// Turns per arm, summed from the plan above.
fn planned_turns(runs: &[(&str, usize)]) -> i64 {
    runs.iter().map(|(_, n)| *n as i64).sum()
}

struct Harness {
    server: TestServer,
    settings: Arc<Settings>,
    db: Arc<dyn DatabaseProvider>,
    mock: MockLlm,
}

/// A server whose only provider is the mock, reached over real HTTP.
///
/// `ProviderRegistry::new` is the production constructor: a provider named
/// `mock` is unknown to it, so it falls through to `OpenAICompatAdapter` and
/// the request leaves through the same code a real deployment uses. Nothing
/// here substitutes a trait impl.
async fn build_harness() -> Harness {
    build_harness_with_retries(default_retries()).await
}

/// The router's own default retry budget, so the common harness stays faithful
/// to a real deployment rather than quietly disabling a production behaviour.
fn default_retries() -> u32 {
    Settings::default().retry.max_retries
}

/// As `build_harness`, with the upstream retry budget pinned. A test that wants
/// an upstream failure to reach the caller needs 0: with retries on, a mock that
/// fails every Nth call is simply retried into success.
async fn build_harness_with_retries(max_retries: u32) -> Harness {
    let mock = MockLlm::start().await;

    let db = common::in_memory_db().await;
    assert_eq!(create_user(&db, "alice", TOKEN).await, 1);
    {
        use modelrouter::db::models::NewAdminUser;
        use modelrouter::db::repositories::admin_users::AdminUserRepository;
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
    }

    let mut settings = Settings {
        pricing: vec![
            PricingEntry {
                model: CONTROL_MODEL.to_string(),
                input_per_million: INPUT_PER_MILLION,
                output_per_million: OUTPUT_PER_MILLION,
                ..Default::default()
            },
            PricingEntry {
                model: CANDIDATE_MODEL.to_string(),
                input_per_million: INPUT_PER_MILLION,
                output_per_million: OUTPUT_PER_MILLION,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    settings.providers.insert(
        "mock".to_string(),
        ProviderConfig {
            api_key: "mock-key".to_string(),
            api_base: Some(mock.base_url()),
            ..Default::default()
        },
    );
    settings.retry.max_retries = max_retries;
    // Retries that do happen must not stall the suite for seconds apiece.
    settings.retry.base_delay_ms = 1;
    settings.retry.max_delay_ms = 5;
    settings.routing.default_model = format!("mock/{CONTROL_MODEL}");
    settings
        .routing
        .model_aliases
        .insert(ALIAS.to_string(), format!("mock/{CONTROL_MODEL}"));
    let settings = Arc::new(settings);

    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&settings.pricing)),
        provider_registry: Arc::new(ProviderRegistry::new(settings.providers.clone())),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
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
        experiments: Arc::new(ExperimentRegistry::default()),
    };

    Harness {
        server: TestServer::new(build_router(state)).unwrap(),
        settings,
        db,
        mock,
    }
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

fn jwt(settings: &Settings, role: &str) -> String {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
    issue_jwt(
        &AdminClaims {
            sub: 1,
            name: format!("{role}-user"),
            role: role.to_string(),
            exp,
        },
        &settings.auth.jwt_secret,
    )
    .unwrap()
}

/// Create the two-arm experiment over the admin API.
async fn create_experiment(h: &Harness) -> i64 {
    let (hk, hv) = bearer(&jwt(&h.settings, "superadmin"));
    let res = h
        .server
        .post("/admin/api/experiments")
        .add_header(hk, hv)
        .json(&json!({
            "name": "terser-but-slower",
            "variants": {
                "control":   { ALIAS: format!("mock/{CONTROL_MODEL}") },
                "candidate": { ALIAS: format!("mock/{CANDIDATE_MODEL}") },
            },
            "expires_at": 0,
            "content_retention_days": 0,
            "retain_content": false
        }))
        .await;
    assert_eq!(res.status_code(), 201, "create failed: {}", res.text());
    res.json::<Value>()["id"].as_i64().unwrap()
}

/// One completion bound to `variant`, carrying correlation id `run`.
async fn complete(h: &Harness, id: i64, variant: &str, run: &str) -> axum_test::TestResponse {
    let (bk, bv) = bearer(TOKEN);
    h.server
        .post("/v1/chat/completions")
        .add_header(bk, bv)
        .add_header(
            axum::http::HeaderName::from_static(EXPERIMENT_HEADER),
            axum::http::HeaderValue::from_str(&format!("{id}:{variant}")).unwrap(),
        )
        .json(&json!({
            "model": ALIAS,
            "messages": [{"role": "user", "content": "plan the week"}],
            "attribution": { "correlation_id": run },
        }))
        .await
}

/// Drive one arm's planned traffic, sequentially so the mock's deterministic
/// jitter sequence is reproducible.
async fn drive(h: &Harness, id: i64, variant: &str, runs: &[(&str, usize)]) {
    for (run, turns) in runs {
        for _ in 0..*turns {
            let res = complete(h, id, variant, run).await;
            assert_eq!(res.status_code(), 200, "{variant}/{run}: {}", res.text());
        }
    }
}

async fn results(h: &Harness, id: i64) -> Value {
    let (hk, hv) = bearer(&jwt(&h.settings, "admin"));
    let res = h
        .server
        .get(&format!("/admin/api/experiments/{id}/results"))
        .add_header(hk, hv)
        .await;
    res.assert_status_ok();
    res.json::<Value>()
}

fn variant<'a>(doc: &'a Value, label: &str) -> &'a Value {
    doc["variants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["label"] == label)
        .unwrap_or_else(|| panic!("no variant {label} in {doc}"))
}

fn run<'a>(doc: &'a Value, correlation_id: &str) -> &'a Value {
    doc["runs"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["correlation_id"] == correlation_id)
        .unwrap_or_else(|| panic!("no run {correlation_id} in {doc}"))
}

fn approx(v: &Value, expected: f64) -> bool {
    (v.as_f64().unwrap() - expected).abs() < 1e-9
}

// ── The oracle ────────────────────────────────────────────────────────────────

/// What the mock actually served for one model, summed independently of the
/// router. This is the authority the results document is checked against.
#[derive(Debug, Default, PartialEq)]
struct Served {
    calls: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
}

impl Served {
    fn of(mock: &MockLlm, model: &str) -> Self {
        mock.served_for(model)
            .iter()
            .filter(|r| r.ok())
            .fold(Served::default(), |mut acc, r| {
                acc.calls += 1;
                acc.prompt_tokens += r.prompt_tokens() as i64;
                acc.completion_tokens += r.completion_tokens() as i64;
                acc
            })
    }

    /// The cost the router should have recorded, from the configured rates.
    fn cost(&self) -> f64 {
        (self.prompt_tokens as f64 / 1_000_000.0) * INPUT_PER_MILLION
            + (self.completion_tokens as f64 / 1_000_000.0) * OUTPUT_PER_MILLION
    }

    fn total_tokens(&self) -> i64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// Distinct completion-token counts the mock served for `model`.
///
/// Guards the trend assertions from being vacuous: if the profile's jitter
/// ever collapsed to a constant, every arm would still aggregate correctly and
/// every comparison would still pass, but the experiment would be comparing two
/// flat lines. A spread of one means the scenario has stopped being a scenario.
fn distinct_completion_sizes(mock: &MockLlm, model: &str) -> usize {
    let mut sizes: Vec<u32> = mock
        .served_for(model)
        .iter()
        .filter(|r| r.ok())
        .map(|r| r.completion_tokens())
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    sizes.len()
}

/// Assert one variant of the results document against what the mock served.
fn assert_variant_matches(v: &Value, served: &Served, model: &str, runs: i64) {
    assert_eq!(v["runs"], runs, "runs");
    assert_eq!(v["turns"], served.calls, "turns");
    assert_eq!(v["requests"], served.calls, "requests");
    assert_eq!(v["mixed_runs"], 0, "no run crossed arms");
    assert_eq!(v["unbound_requests"], 0, "every turn carried the header");
    assert_eq!(v["failures"], 0, "no upstream failures in this scenario");

    assert_eq!(v["tokens"]["prompt"], served.prompt_tokens, "prompt tokens");
    assert_eq!(v["tokens"]["completion"], served.completion_tokens, "completion tokens");
    assert_eq!(v["tokens"]["total"], served.total_tokens(), "total tokens");

    assert!(
        approx(&v["cost_usd"], served.cost()),
        "cost {} != {} priced from what the mock served",
        v["cost_usd"],
        served.cost()
    );
    assert_eq!(v["unpriced"], false, "both models are priced");
    assert_eq!(v["estimated_rows"], 0, "the mock reports usage, so nothing is estimated");

    // The model breakdown names the pinned model, not the alias asked for.
    let models = v["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "one arm, one model: {models:?}");
    assert_eq!(models[0]["model"], model);
    assert_eq!(models[0]["requests"], served.calls);
    assert_eq!(models[0]["tokens"]["total"], served.total_tokens());

    // Every turn wrote a prompt row carrying a real measurement.
    assert_eq!(v["latency_samples"], served.calls, "one latency sample per turn");
    assert!(v["latency"]["mean_ms"].as_f64().unwrap() > 0.0, "mean latency is measured");

    // Per-request figures are the totals divided by the request count.
    let per_request = &v["per_request"];
    assert!(approx(&per_request["cost_usd"], served.cost() / served.calls as f64));
    assert!(approx(
        &per_request["tokens_in"],
        served.prompt_tokens as f64 / served.calls as f64
    ));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// The whole loop: create over HTTP, run traffic through the real adapter,
/// report an outcome, read the results back, and check every figure against
/// what the mock served.
#[tokio::test]
async fn a_mock_experiment_runs_end_to_end_and_the_api_reports_what_the_mock_served() {
    let h = build_harness().await;
    // Control is chatty and quick; candidate is terser but slower.
    h.mock.set_profile(
        CONTROL_MODEL,
        ModelProfile {
            prompt_tokens: (200, 40),
            completion_tokens: (140, 30),
            latency_ms: (4, 2),
            fail_every: 0,
        },
    );
    h.mock.set_profile(
        CANDIDATE_MODEL,
        ModelProfile {
            prompt_tokens: (150, 30),
            completion_tokens: (60, 20),
            latency_ms: (28, 8),
            fail_every: 0,
        },
    );

    let id = create_experiment(&h).await;
    drive(&h, id, "control", CONTROL_RUNS).await;
    drive(&h, id, "candidate", CANDIDATE_RUNS).await;

    let turns = planned_turns(CONTROL_RUNS) + planned_turns(CANDIDATE_RUNS);
    common::wait_for_ledger_rows(&*h.db, turns as usize).await;

    // The overlay reached the real adapter: the mock was asked for the pinned
    // models, never for the alias.
    let control_served = Served::of(&h.mock, CONTROL_MODEL);
    let candidate_served = Served::of(&h.mock, CANDIDATE_MODEL);
    assert_eq!(control_served.calls, planned_turns(CONTROL_RUNS));
    assert_eq!(candidate_served.calls, planned_turns(CANDIDATE_RUNS));
    assert!(
        h.mock.served_for(ALIAS).is_empty(),
        "the provider must never be asked for the alias"
    );

    let doc = results(&h, id).await;

    assert_variant_matches(
        variant(&doc, "control"),
        &control_served,
        CONTROL_MODEL,
        CONTROL_RUNS.len() as i64,
    );
    assert_variant_matches(
        variant(&doc, "candidate"),
        &candidate_served,
        CANDIDATE_MODEL,
        CANDIDATE_RUNS.len() as i64,
    );

    // Experiment totals are the two arms summed.
    let totals = &doc["totals"];
    assert_eq!(totals["runs"], (CONTROL_RUNS.len() + CANDIDATE_RUNS.len()) as i64);
    assert_eq!(totals["turns"], turns);
    assert_eq!(
        totals["tokens"]["total"],
        control_served.total_tokens() + candidate_served.total_tokens()
    );
    assert!(approx(
        &totals["cost_usd"],
        control_served.cost() + candidate_served.cost()
    ));

    // Every planned run is listed, under its own arm and turn count.
    assert_eq!(doc["runs"]["total"], (CONTROL_RUNS.len() + CANDIDATE_RUNS.len()) as i64);
    for (correlation_id, planned) in CONTROL_RUNS {
        let r = run(&doc, correlation_id);
        assert_eq!(r["variant"], "control", "{correlation_id}");
        assert_eq!(r["turns"], *planned as i64, "{correlation_id}");
        assert_eq!(r["mixed"], false, "{correlation_id}");
        assert_eq!(r["user_id"], 1, "{correlation_id}");
    }
    for (correlation_id, planned) in CANDIDATE_RUNS {
        let r = run(&doc, correlation_id);
        assert_eq!(r["variant"], "candidate", "{correlation_id}");
        assert_eq!(r["turns"], *planned as i64, "{correlation_id}");
    }
}

/// The trend the experiment exists to detect: the candidate spends fewer
/// tokens per request and costs less, but is slower. Asserted as a direction,
/// not a literal, since the mock's jitter moves the exact figures.
#[tokio::test]
async fn the_results_show_the_candidate_is_cheaper_per_request_but_slower() {
    let h = build_harness().await;
    h.mock.set_profile(
        CONTROL_MODEL,
        ModelProfile {
            prompt_tokens: (200, 40),
            completion_tokens: (140, 30),
            latency_ms: (4, 2),
            fail_every: 0,
        },
    );
    h.mock.set_profile(
        CANDIDATE_MODEL,
        ModelProfile {
            prompt_tokens: (150, 30),
            completion_tokens: (60, 20),
            latency_ms: (28, 8),
            fail_every: 0,
        },
    );

    let id = create_experiment(&h).await;
    drive(&h, id, "control", CONTROL_RUNS).await;
    drive(&h, id, "candidate", CANDIDATE_RUNS).await;
    let turns = planned_turns(CONTROL_RUNS) + planned_turns(CANDIDATE_RUNS);
    common::wait_for_ledger_rows(&*h.db, turns as usize).await;

    // The profiles must actually vary, or the comparison below is between two
    // constants and proves nothing about aggregation over a spread.
    assert!(
        distinct_completion_sizes(&h.mock, CONTROL_MODEL) > 1,
        "control's jitter collapsed to a constant"
    );
    assert!(
        distinct_completion_sizes(&h.mock, CANDIDATE_MODEL) > 1,
        "candidate's jitter collapsed to a constant"
    );

    let doc = results(&h, id).await;
    let control = variant(&doc, "control");
    let candidate = variant(&doc, "candidate");

    // Fewer tokens, and therefore — at equal rates — less money.
    let control_tokens = control["tokens"]["total"].as_i64().unwrap();
    let candidate_tokens = candidate["tokens"]["total"].as_i64().unwrap();
    assert!(
        candidate_tokens < control_tokens,
        "candidate should be terser: {candidate_tokens} vs {control_tokens}"
    );
    let control_cost = control["cost_usd"].as_f64().unwrap();
    let candidate_cost = candidate["cost_usd"].as_f64().unwrap();
    assert!(
        candidate_cost < control_cost,
        "candidate should be cheaper: {candidate_cost} vs {control_cost}"
    );

    // Same traffic shape on both arms, so per-request cost moves the same way.
    let control_per_req = control["per_request"]["cost_usd"].as_f64().unwrap();
    let candidate_per_req = candidate["per_request"]["cost_usd"].as_f64().unwrap();
    assert!(
        candidate_per_req < control_per_req,
        "candidate should be cheaper per request: {candidate_per_req} vs {control_per_req}"
    );

    // ...but it is slower, which is the trade the experiment surfaces.
    let control_mean = control["latency"]["mean_ms"].as_f64().unwrap();
    let candidate_mean = candidate["latency"]["mean_ms"].as_f64().unwrap();
    assert!(
        candidate_mean > control_mean,
        "candidate should be slower: {candidate_mean} ms vs {control_mean} ms"
    );
    // Percentiles are populated from the same rows, so they order too.
    assert!(
        candidate["latency"]["p50_ms"].as_i64().unwrap()
            >= control["latency"]["p50_ms"].as_i64().unwrap(),
        "candidate p50 should not be faster"
    );
}

/// Outcomes reported by the caller over `/v1/feedback` reach the per-variant
/// figures, so an experiment can be judged on quality and not only on cost.
#[tokio::test]
async fn reported_outcomes_reach_the_variant_figures() {
    let h = build_harness().await;
    h.mock.set_profile(CONTROL_MODEL, ModelProfile::default());
    h.mock.set_profile(CANDIDATE_MODEL, ModelProfile::default());

    let id = create_experiment(&h).await;
    drive(&h, id, "control", CONTROL_RUNS).await;
    drive(&h, id, "candidate", CANDIDATE_RUNS).await;
    let turns = planned_turns(CONTROL_RUNS) + planned_turns(CANDIDATE_RUNS);
    common::wait_for_ledger_rows(&*h.db, turns as usize).await;

    // Control: three successes. Candidate: two successes and one failure —
    // terser answers that more often missed.
    let report = |run: &'static str, outcome: &'static str, score: f64| {
        let server = &h.server;
        async move {
            let (bk, bv) = bearer(TOKEN);
            let res = server
                .post("/v1/feedback")
                .add_header(bk, bv)
                .json(&json!({
                    "correlation_id": run,
                    "outcome": outcome,
                    "score": score,
                }))
                .await;
            assert_eq!(res.status_code(), 200, "{run}: {}", res.text());
        }
    };
    report("c1", "success", 0.9).await;
    report("c2", "success", 0.8).await;
    report("c3", "success", 0.7).await;
    report("k1", "success", 0.6).await;
    report("k2", "success", 0.5).await;
    report("k3", "failure", 0.1).await;

    let doc = results(&h, id).await;

    let control = variant(&doc, "control")["outcomes"].clone();
    assert_eq!(control["reported"], 3);
    assert_eq!(control["success"], 3);
    assert_eq!(control["failure"], 0);
    assert!(approx(&control["success_rate"], 1.0));
    assert!(approx(&control["mean_score"], (0.9 + 0.8 + 0.7) / 3.0));
    assert_eq!(control["score_samples"], 3);

    let candidate = variant(&doc, "candidate")["outcomes"].clone();
    assert_eq!(candidate["reported"], 3);
    assert_eq!(candidate["success"], 2);
    assert_eq!(candidate["failure"], 1);
    assert!(approx(&candidate["success_rate"], 2.0 / 3.0));
    assert!(approx(&candidate["mean_score"], (0.6 + 0.5 + 0.1) / 3.0));

    // The unreported run of each arm is still a run, just without an outcome.
    assert_eq!(doc["totals"]["outcomes"]["reported"], 6);
    assert!(run(&doc, "c4")["outcome"].is_null());
    assert!(run(&doc, "k4")["outcome"].is_null());

    // A reported run carries its outcome inline.
    assert_eq!(run(&doc, "c1")["outcome"]["outcome"], "success");
    assert!(approx(&run(&doc, "k3")["outcome"]["score"], 0.1));
}

/// An upstream that fails part of the time is reported as failures against the
/// arm that hit it, while the turns that succeeded still aggregate normally.
#[tokio::test]
async fn upstream_failures_are_counted_against_the_arm_that_hit_them() {
    // No retry budget: a failing turn must reach the caller to be counted as a
    // failure rather than being retried into a success.
    let h = build_harness_with_retries(0).await;
    h.mock.set_profile(CONTROL_MODEL, ModelProfile::default());
    // Every third call to the candidate's model fails.
    h.mock.set_profile(
        CANDIDATE_MODEL,
        ModelProfile { fail_every: 3, ..ModelProfile::default() },
    );

    let id = create_experiment(&h).await;
    drive(&h, id, "control", CONTROL_RUNS).await;

    // The candidate's failing turns surface to the caller as 502s.
    let mut ok = 0;
    let mut failed = 0;
    for (run, turns) in CANDIDATE_RUNS {
        for _ in 0..*turns {
            let res = complete(&h, id, "candidate", run).await;
            match res.status_code().as_u16() {
                200 => ok += 1,
                502 => failed += 1,
                other => panic!("unexpected {other} for {run}: {}", res.text()),
            }
        }
    }
    assert!(failed > 0, "the profile should have failed some candidate turns");
    assert_eq!(ok + failed, planned_turns(CANDIDATE_RUNS));

    let control_turns = planned_turns(CONTROL_RUNS);
    common::wait_for_ledger_rows(&*h.db, (control_turns + ok) as usize).await;

    let doc = results(&h, id).await;
    let control = variant(&doc, "control");
    let candidate = variant(&doc, "candidate");

    // Control was untouched by the failing model.
    assert_eq!(control["failures"], 0);
    assert_eq!(control["turns"], control_turns);

    // The candidate's ledger rows are only its successful turns; the failures
    // are counted separately rather than silently dropped.
    assert_eq!(candidate["turns"], ok, "only successful turns reach the ledger");
    assert_eq!(candidate["failures"], failed, "failed turns are counted");
    assert_eq!(doc["totals"]["failures"], failed);

    // A failed turn still bills nothing, so the arm's cost matches what the
    // mock actually served.
    let served = Served::of(&h.mock, CANDIDATE_MODEL);
    assert_eq!(served.calls, ok, "the oracle counts only served completions");
    assert!(approx(&candidate["cost_usd"], served.cost()));
}

/// A flaky upstream under the router's normal retry budget is recovered before
/// the caller ever sees it, and the experiment records the turn as the success
/// it ended up being.
///
/// This is the counterpart to the test above and the reason it has to pin
/// `max_retries` to 0: with the default budget, a model that fails every third
/// call still answers every turn, so the arm shows full turns and no failures
/// while the mock's own record shows the extra attempts.
#[tokio::test]
async fn retries_hide_a_flaky_upstream_and_the_arm_still_reconciles() {
    let h = build_harness().await;
    h.mock.set_profile(CONTROL_MODEL, ModelProfile::default());
    h.mock.set_profile(
        CANDIDATE_MODEL,
        ModelProfile { fail_every: 3, ..ModelProfile::default() },
    );

    let id = create_experiment(&h).await;
    drive(&h, id, "candidate", CANDIDATE_RUNS).await; // every turn returns 200

    let planned = planned_turns(CANDIDATE_RUNS);
    common::wait_for_ledger_rows(&*h.db, planned as usize).await;

    // The mock was hit more often than the caller asked, because some calls
    // were retried.
    let attempts = h.mock.served_for(CANDIDATE_MODEL).len() as i64;
    let refused = h
        .mock
        .served_for(CANDIDATE_MODEL)
        .iter()
        .filter(|r| !r.ok())
        .count() as i64;
    assert!(refused > 0, "the profile should have refused some attempts");
    assert_eq!(attempts, planned + refused, "each refusal cost one extra attempt");

    let doc = results(&h, id).await;
    let candidate = variant(&doc, "candidate");
    assert_eq!(candidate["turns"], planned, "every turn ultimately succeeded");
    assert_eq!(candidate["failures"], 0, "a recovered turn is not a failure");

    // Only the successful attempts were billed, and the arm's cost is exactly
    // the sum of what the mock served.
    let served = Served::of(&h.mock, CANDIDATE_MODEL);
    assert_eq!(served.calls, planned);
    assert!(approx(&candidate["cost_usd"], served.cost()));
}
