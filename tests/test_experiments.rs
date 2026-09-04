//! Controlled experiments on the request path (spec §7a).
//!
//! `x-modelrouter-experiment` on `/v1/chat/completions` binds the request to
//! a variant: the variant's overlay pins the requested model to a concrete
//! `provider/model`, the adaptive layers stand aside, and every row the
//! request writes carries the experiment id and variant. Every other `/v1`
//! endpoint refuses the header.

// Only the embedding and search mocks are used here; the provider is local.
#[allow(dead_code)]
mod common;

use common::create_user;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::callbacks::{CallbackBackend, CallbackDispatcher, CallbackEvent};
use modelrouter::config::schema::{
    CacheConfig, LbPoolEntry, LbStrategy, LoadBalancerConfig, PricingEntry, Settings,
    StorageConfig,
};
use modelrouter::db::models::{
    CostLedgerEntry, ExperimentVariants, NewExperiment, Prompt, RequestFailure, RunStamp,
    VariantTarget,
};
use modelrouter::db::prompt_store::CONTENT_NOT_STORED;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::experiments::ExperimentRepository;
use modelrouter::db::repositories::failures::FailureRepository;
use modelrouter::db::repositories::prompts::PromptRepository;
use modelrouter::providers::adapter::{
    CompletionResult, NormalizedRequest, ProviderAdapter, SseStream,
};
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search_registry::SearchRegistry,
};
use modelrouter::router::experiments::{ExperimentRegistry, EXPERIMENT_HEADER};
use modelrouter::router::session_affinity::SessionAffinityMap;
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator,
    engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};

/// Bearer token of the first user (id 1).
const TOKEN_A: &str = "token-a";
/// Bearer token of the second user (id 2).
const TOKEN_B: &str = "token-b";

/// A provider that records the model of every call and fails on demand.
/// Registered as the only adapter, so every provider name reaches it.
struct RecordingAdapter {
    calls: Arc<Mutex<Vec<String>>>,
    fail_models: Arc<Mutex<HashSet<String>>>,
}

impl RecordingAdapter {
    fn check(&self, req: &NormalizedRequest) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(req.model.clone());
        if self.fail_models.lock().unwrap().contains(&req.model) {
            // Not a status code the retry policy recognises, so no backoff.
            anyhow::bail!("mock upstream refused {}", req.model);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for RecordingAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        self.check(req)?;
        Ok(CompletionResult {
            content: format!("answer from {}", req.model),
            prompt_tokens: 10,
            completion_tokens: 20,
            finish_reason: "stop".to_string(),
            ..Default::default()
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        use bytes::Bytes;
        use futures::stream;
        self.check(req)?;
        let data = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"answer from {}\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],\
             \"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":20}}}}\n\ndata: [DONE]\n\n",
            req.model
        );
        let stream = stream::once(async move { Ok::<Bytes, anyhow::Error>(Bytes::from(data)) });
        Ok(Box::pin(stream))
    }
}

/// A callback backend that keeps every event it is handed, so a test can
/// assert that the observability egress stayed shut.
struct RecordingBackend {
    events: Arc<Mutex<Vec<CallbackEvent>>>,
}

impl CallbackBackend for RecordingBackend {
    fn send(&self, event: CallbackEvent) {
        self.events.lock().unwrap().push(event);
    }
}

struct Harness {
    server: TestServer,
    db: Arc<dyn DatabaseProvider>,
    calls: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<CallbackEvent>>>,
    fail_models: Arc<Mutex<HashSet<String>>>,
    router: Arc<RequestRouter>,
    session_affinity: Arc<SessionAffinityMap>,
    experiments: Arc<ExperimentRegistry>,
}

impl Harness {
    /// Models the provider has been asked for, in order.
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn fail(&self, model: &str) {
        self.fail_models.lock().unwrap().insert(model.to_string());
    }

    /// Seed one experiment: `candidate` pins `planner` to the mock's second
    /// model, `control` has an empty overlay. Returns its id.
    async fn seed_experiment(&self, allowed: Vec<i64>, retain_content: bool) -> i64 {
        let mut candidate = BTreeMap::new();
        candidate.insert(
            "planner".to_string(),
            VariantTarget {
                target: "mock/model-b".to_string(),
                provider: "mock".to_string(),
                model: "model-b".to_string(),
            },
        );
        let mut variants: ExperimentVariants = BTreeMap::new();
        variants.insert("candidate".to_string(), candidate);
        variants.insert("control".to_string(), BTreeMap::new());
        let created = ExperimentRepository::create(
            &*self.db,
            NewExperiment {
                name: format!("exp-{}", self.experiments.len() + 1),
                variants,
                allowed_user_ids: allowed,
                feed_learning: false,
                expires_at: 0,
                retain_content,
                content_retention_days: 0,
            },
        )
        .await
        .unwrap();
        self.experiments.load_from(&*self.db).await.unwrap();
        created.id
    }

    async fn close_experiment(&self, id: i64) {
        assert!(ExperimentRepository::close(&*self.db, id, "2026-09-02T00:00:00+00:00")
            .await
            .unwrap());
        self.experiments.load_from(&*self.db).await.unwrap();
    }

    /// Cost logging is fire-and-forget, so poll rather than sleep.
    async fn wait_for_ledger_rows(&self, want: usize) -> Vec<CostLedgerEntry> {
        common::wait_for_ledger_rows(&*self.db, want).await
    }

    /// The ledger stamp for one run. Used where a row may carry no prompt id,
    /// which `list_cost_entries_before` cannot read back.
    async fn wait_for_run(&self, user_id: i64, correlation_id: &str) -> RunStamp {
        for _ in 0..200 {
            if let Some(stamp) = CostRepository::run_stamp(&*self.db, user_id, correlation_id)
                .await
                .unwrap()
            {
                return stamp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for the ledger row of {correlation_id}");
    }

    async fn wait_for_failure_rows(&self, want: usize) -> Vec<RequestFailure> {
        for _ in 0..200 {
            let rows = FailureRepository::list(&*self.db, 100, 0).await.unwrap();
            if rows.len() >= want {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {want} failure rows");
    }

    async fn prompts(&self) -> Vec<Prompt> {
        PromptRepository::list(&*self.db, 100, 0).await.unwrap()
    }

    /// Callback events dispatched so far.
    fn events(&self) -> Vec<CallbackEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Dispatch follows the ledger write in the same task, so poll briefly
    /// rather than assume the row and the event land together.
    async fn wait_for_events(&self, want: usize) -> Vec<CallbackEvent> {
        for _ in 0..200 {
            let events = self.events();
            if events.len() >= want {
                return events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {want} callback events");
    }

    /// The prompt row of one run; call after `wait_for_run`, which follows the
    /// row insert on every logging path.
    async fn prompt_for_run(&self, correlation_id: &str) -> Prompt {
        self.prompts()
            .await
            .into_iter()
            .find(|p| p.attribution_correlation_id.as_deref() == Some(correlation_id))
            .unwrap_or_else(|| panic!("no prompt row for {correlation_id}"))
    }
}

struct Options {
    cache: bool,
    store_prompts: bool,
    store_prompt_content: bool,
    pool: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cache: false,
            store_prompts: true,
            store_prompt_content: true,
            pool: false,
        }
    }
}

/// Two users, a priced alias `planner` -> `mock/model-a`, a fallback chain
/// from `model-b` to `model-a` (so a bound request's refusal to fall back is
/// observable), and optionally a pool, a cache and prompt storage off.
async fn build_app(opts: Options) -> Harness {
    let db = common::in_memory_db().await;
    assert_eq!(create_user(&db, "user-a", TOKEN_A).await, 1);
    assert_eq!(create_user(&db, "user-b", TOKEN_B).await, 2);

    let mut settings = Settings {
        pricing: vec![
            PricingEntry {
                model: "model-a".to_string(),
                input_per_million: 1000.0,
                output_per_million: 1000.0,
                ..Default::default()
            },
            PricingEntry {
                model: "model-b".to_string(),
                input_per_million: 2000.0,
                output_per_million: 2000.0,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    settings.routing.default_model = "mock/model-a".to_string();
    settings
        .routing
        .model_aliases
        .insert("planner".to_string(), "mock/model-a".to_string());
    settings
        .routing
        .fallback_chains
        .insert("model-b".to_string(), vec!["mock/model-a".to_string()]);
    if opts.pool {
        settings.routing.load_balancer.insert(
            "pool".to_string(),
            LoadBalancerConfig {
                strategy: LbStrategy::RoundRobin,
                pool: vec![LbPoolEntry {
                    provider: "mock".to_string(),
                    model: "model-a".to_string(),
                    weight: 1,
                }],
            },
        );
    }
    let settings = Arc::new(settings);
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);

    let calls = Arc::new(Mutex::new(Vec::new()));
    let fail_models = Arc::new(Mutex::new(HashSet::new()));
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let session_affinity = Arc::new(SessionAffinityMap::new(1800));
    let experiments = Arc::new(ExperimentRegistry::default());
    let storage = StorageConfig {
        store_prompts: opts.store_prompts,
        store_prompt_content: opts.store_prompt_content,
        ..Default::default()
    };
    let events = Arc::new(Mutex::new(Vec::new()));

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: router.clone(),
        cost_calc: Arc::new(CostCalculator::new_with_config(&settings.pricing)),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(RecordingAdapter {
            calls: calls.clone(),
            fail_models: fail_models.clone(),
        })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(settings.routing.fallback_chains.clone())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig {
            enabled: opts.cache,
            max_entries: 10,
            ttl_seconds: 60,
            ..Default::default()
        })),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter {
                embedding: vec![0.1_f32, 0.2],
            },
        )),
        search_registry: Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter {
            results: vec![],
        })),
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            settings.routing.load_balancer.clone(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(
            modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0),
        ),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: session_affinity.clone(),
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(storage)),
        prompt_db: db.clone(),
        app_metrics: None,
        callbacks: Arc::new(CallbackDispatcher::new(vec![Box::new(RecordingBackend {
            events: events.clone(),
        })])),
        guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: experiments.clone(),
    };
    Harness {
        server: TestServer::new(build_router(state)).unwrap(),
        db,
        calls,
        events,
        fail_models,
        router,
        session_affinity,
        experiments,
    }
}

fn bearer(token: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    )
}

fn experiment_header(value: &str) -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        axum::http::HeaderName::from_static(EXPERIMENT_HEADER),
        axum::http::HeaderValue::from_str(value).unwrap(),
    )
}

/// A chat completion body for `model` carrying correlation id `run`.
fn chat_body(model: &str, run: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "plan the week"}],
        "attribution": { "correlation_id": run },
    })
}

/// POST a completion, optionally under the experiment header.
async fn complete(
    h: &Harness,
    token: &str,
    header: Option<&str>,
    body: &Value,
) -> axum_test::TestResponse {
    let mut req = h
        .server
        .post("/v1/chat/completions")
        .add_header(bearer(token).0, bearer(token).1);
    if let Some(value) = header {
        let (name, value) = experiment_header(value);
        req = req.add_header(name, value);
    }
    req.json(body).await
}

#[tokio::test]
async fn bound_request_is_answered_by_the_pinned_model_and_stamped() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;

    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{id}:candidate")),
        &chat_body("planner", "run-1"),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: Value = resp.json();
    assert_eq!(body["model"], "model-b");
    assert_eq!(body["choices"][0]["message"]["content"], "answer from model-b");
    assert_eq!(h.calls(), vec!["model-b"]);

    let ledger = h.wait_for_ledger_rows(1).await;
    assert_eq!(ledger[0].model, "model-b");
    assert_eq!(ledger[0].provider, "mock");
    assert_eq!(ledger[0].experiment_id, Some(id));
    assert_eq!(ledger[0].experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(ledger[0].attribution_correlation_id.as_deref(), Some("run-1"));
    // Priced at model-b's rate: 10 in + 20 out at $2000/M.
    assert!((ledger[0].cost_usd - 0.06).abs() < 1e-9);

    let prompts = h.prompts().await;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].request_model, "planner");
    assert_eq!(prompts[0].routed_model, "model-b");
    assert_eq!(prompts[0].experiment_id, Some(id));
    assert_eq!(prompts[0].experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(prompts[0].attribution_correlation_id.as_deref(), Some("run-1"));
    assert_eq!(ledger[0].prompt_id, prompts[0].id);
}

#[tokio::test]
async fn unbound_request_routes_to_the_alias_target_unstamped() {
    let h = build_app(Options::default()).await;
    h.seed_experiment(vec![], false).await;

    let resp = complete(&h, TOKEN_A, None, &chat_body("planner", "run-1")).await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.json::<Value>()["model"], "model-a");
    assert_eq!(h.calls(), vec!["model-a"]);

    let ledger = h.wait_for_ledger_rows(1).await;
    assert_eq!(ledger[0].model, "model-a");
    assert_eq!(ledger[0].experiment_id, None);
    assert_eq!(ledger[0].experiment_variant, None);
    let prompts = h.prompts().await;
    assert_eq!(prompts[0].experiment_id, None);
    assert_eq!(prompts[0].experiment_variant, None);
}

#[tokio::test]
async fn empty_overlay_variant_routes_normally_but_is_stamped() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;

    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{id}:control")),
        &chat_body("planner", "run-1"),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(resp.json::<Value>()["model"], "model-a");
    assert_eq!(h.calls(), vec!["model-a"]);

    let ledger = h.wait_for_ledger_rows(1).await;
    assert_eq!(ledger[0].model, "model-a");
    assert_eq!(ledger[0].experiment_id, Some(id));
    assert_eq!(ledger[0].experiment_variant.as_deref(), Some("control"));
    let prompts = h.prompts().await;
    assert_eq!(prompts[0].experiment_id, Some(id));
    assert_eq!(prompts[0].experiment_variant.as_deref(), Some("control"));
}

#[tokio::test]
async fn id_only_binding_lands_on_one_variant_for_the_whole_session() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;

    let mut body = chat_body("planner", "run-1");
    body["session_id"] = json!("session-42");
    for _ in 0..5 {
        let resp = complete(&h, TOKEN_A, Some(&id.to_string()), &body).await;
        assert_eq!(resp.status_code(), 200, "{}", resp.text());
    }

    let ledger = h.wait_for_ledger_rows(5).await;
    let variants: HashSet<Option<String>> =
        ledger.iter().map(|r| r.experiment_variant.clone()).collect();
    assert_eq!(variants.len(), 1, "one session, one variant: {variants:?}");
    let variant = variants.into_iter().next().unwrap().expect("stamped");
    assert!(variant == "candidate" || variant == "control");
    assert!(ledger.iter().all(|r| r.experiment_id == Some(id)));

    // The model follows the variant on every request.
    let expected = if variant == "candidate" { "model-b" } else { "model-a" };
    assert_eq!(h.calls(), vec![expected; 5]);

    // Another session may land elsewhere, and does over enough of them.
    let mut seen = HashSet::new();
    for i in 0..40 {
        body["session_id"] = json!(format!("s-{i}"));
        let resp = complete(&h, TOKEN_A, Some(&id.to_string()), &body).await;
        assert_eq!(resp.status_code(), 200);
        seen.insert(resp.json::<Value>()["model"].as_str().unwrap().to_string());
    }
    assert_eq!(seen.len(), 2, "both variants reached across sessions");
}

#[tokio::test]
async fn bound_request_neither_reads_nor_writes_an_affinity_pin() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;

    // A pin left by an earlier unbound request must not steer the bound one.
    h.session_affinity.set("session-7", "mock", "model-a");
    let mut body = chat_body("planner", "run-1");
    body["session_id"] = json!("session-7");
    let resp = complete(&h, TOKEN_A, Some(&format!("{id}:candidate")), &body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(resp.json::<Value>()["model"], "model-b");
    let pin = h.session_affinity.get("session-7").expect("pin untouched");
    assert_eq!(pin.model, "model-a");

    // And a fresh session under a binding creates no pin at all.
    body["session_id"] = json!("session-8");
    let resp = complete(&h, TOKEN_A, Some(&format!("{id}:candidate")), &body).await;
    assert_eq!(resp.status_code(), 200);
    assert!(h.session_affinity.get("session-8").is_none());
    assert_eq!(h.session_affinity.len(), 1);

    // Whereas the same request unbound pins the session.
    let resp = complete(&h, TOKEN_A, None, &body).await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(h.session_affinity.get("session-8").unwrap().model, "model-a");
}

#[tokio::test]
async fn bound_request_is_not_served_from_cache() {
    let h = build_app(Options {
        cache: true,
        ..Default::default()
    })
    .await;
    let id = h.seed_experiment(vec![], false).await;
    // Deterministic, so the default cache policy admits it.
    let mut body = chat_body("planner", "run-1");
    body["temperature"] = json!(0.0);

    // Warm the cache with the unbound request; the second unbound call hits.
    let first = complete(&h, TOKEN_A, None, &body).await;
    assert_eq!(first.header("x-modelrouter-cache"), "MISS");
    let second = complete(&h, TOKEN_A, None, &body).await;
    assert_eq!(second.header("x-modelrouter-cache"), "HIT");
    assert_eq!(h.calls().len(), 1);

    // The control variant resolves to the very same model, so the key would
    // match; the binding still goes to the provider.
    let bound = complete(&h, TOKEN_A, Some(&format!("{id}:control")), &body).await;
    assert_eq!(bound.status_code(), 200, "{}", bound.text());
    assert_eq!(bound.header("x-modelrouter-cache"), "MISS");
    assert_eq!(h.calls().len(), 2);

    // Nor does the bound response feed the cache: the next unbound call is
    // served from the entry the first call wrote, and a bound repeat misses
    // again.
    let again = complete(&h, TOKEN_A, Some(&format!("{id}:control")), &body).await;
    assert_eq!(again.header("x-modelrouter-cache"), "MISS");
    assert_eq!(h.calls().len(), 3);
}

#[tokio::test]
async fn provider_failure_under_a_binding_is_a_502_with_no_fallback() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;
    h.fail("model-b");

    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{id}:candidate")),
        &chat_body("planner", "run-9"),
    )
    .await;
    assert_eq!(resp.status_code(), 502, "{}", resp.text());
    // Only the pinned model was tried; the chain to model-a was not followed.
    assert_eq!(h.calls(), vec!["model-b"]);

    let failures = h.wait_for_failure_rows(1).await;
    let f = &failures[0];
    assert_eq!(f.stage, "provider");
    assert_eq!(f.status_code, Some(502));
    assert_eq!(f.request_model, "planner");
    assert_eq!(f.routed_model.as_deref(), Some("model-b"));
    assert_eq!(f.provider.as_deref(), Some("mock"));
    assert_eq!(f.experiment_id, Some(id));
    assert_eq!(f.experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(f.attribution_correlation_id.as_deref(), Some("run-9"));

    // The same failure unbound follows the chain and succeeds on model-a.
    let resp = complete(&h, TOKEN_A, None, &chat_body("mock/model-b", "run-10")).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(resp.json::<Value>()["model"], "model-a");
    assert_eq!(h.calls(), vec!["model-b", "model-b", "model-a"]);
}

#[tokio::test]
async fn bind_rejections_are_400s_that_never_reach_the_provider() {
    let h = build_app(Options::default()).await;
    let open = h.seed_experiment(vec![], false).await;
    let gated = h.seed_experiment(vec![1], false).await;
    let closed = h.seed_experiment(vec![], false).await;
    h.close_experiment(closed).await;

    let cases: Vec<(String, &str, Value, &str)> = vec![
        (
            format!("{closed}:candidate"),
            TOKEN_A,
            chat_body("planner", "run-1"),
            "is closed",
        ),
        (
            format!("{open}:nope"),
            TOKEN_A,
            chat_body("planner", "run-1"),
            "no variant 'nope'",
        ),
        (
            format!("{open}:candidate"),
            TOKEN_A,
            json!({"model": "planner", "messages": [{"role": "user", "content": "hi"}]}),
            "correlation_id is required",
        ),
        (
            format!("{gated}:candidate"),
            TOKEN_B,
            chat_body("planner", "run-1"),
            "user 2 is not allowed",
        ),
        (
            "99:candidate".to_string(),
            TOKEN_A,
            chat_body("planner", "run-1"),
            "experiment 99 not found",
        ),
    ];
    for (header, token, body, expect) in &cases {
        let resp = complete(&h, token, Some(header), body).await;
        assert_eq!(resp.status_code(), 400, "{header}: {}", resp.text());
        let text = resp.text();
        assert!(text.contains(expect), "{header}: {text}");
    }
    assert!(h.calls().is_empty(), "no rejected request reached the provider");

    // Each rejection is an ordinary request-stage failure row, unstamped.
    let failures = h.wait_for_failure_rows(cases.len()).await;
    assert_eq!(failures.len(), cases.len());
    for f in &failures {
        assert_eq!(f.stage, "request");
        assert_eq!(f.status_code, Some(400));
        assert_eq!(f.experiment_id, None);
        assert_eq!(f.experiment_variant, None);
    }
    // The allowed user binds fine.
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{gated}:candidate")),
        &chat_body("planner", "run-1"),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn header_is_refused_on_every_other_v1_endpoint() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;
    let header = format!("{id}:candidate");

    let posts: Vec<(&str, Value)> = vec![
        (
            "/v1/messages",
            json!({"model": "planner", "max_tokens": 10,
                   "messages": [{"role": "user", "content": "hi"}]}),
        ),
        ("/v1/search", json!({"query": "hi"})),
        ("/v1/feedback", json!({"correlation_id": "run-1", "outcome": "success"})),
        ("/v1/embeddings", json!({"model": "planner", "input": ["hi"]})),
        (
            "/v1/responses",
            json!({"model": "planner", "input": "hi"}),
        ),
    ];
    for (path, body) in posts {
        let resp = h
            .server
            .post(path)
            .add_header(bearer(TOKEN_A).0, bearer(TOKEN_A).1)
            .add_header(experiment_header(&header).0, experiment_header(&header).1)
            .json(&body)
            .await;
        assert_eq!(resp.status_code(), 400, "{path}: {}", resp.text());
        assert!(
            resp.text().contains("not supported on this endpoint"),
            "{path}: {}",
            resp.text()
        );
    }
    let resp = h
        .server
        .get("/v1/models")
        .add_header(bearer(TOKEN_A).0, bearer(TOKEN_A).1)
        .add_header(experiment_header(&header).0, experiment_header(&header).1)
        .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    assert!(resp.text().contains("not supported on this endpoint"));

    // Without the header the same endpoint answers.
    let resp = h
        .server
        .get("/v1/models")
        .add_header(bearer(TOKEN_A).0, bearer(TOKEN_A).1)
        .await;
    assert_eq!(resp.status_code(), 200);
    assert!(h.calls().is_empty());
}

#[tokio::test]
async fn streaming_bound_request_stamps_its_rows_once_the_stream_ends() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;

    let mut body = chat_body("planner", "run-stream");
    body["stream"] = json!(true);
    let resp = complete(&h, TOKEN_A, Some(&format!("{id}:candidate")), &body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    // The test server reads the whole body, so `[DONE]` has passed the logger.
    let text = resp.text();
    assert!(text.contains("answer from model-b"), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");
    assert_eq!(h.calls(), vec!["model-b"]);

    let ledger = h.wait_for_ledger_rows(1).await;
    assert_eq!(ledger[0].model, "model-b");
    assert_eq!(ledger[0].experiment_id, Some(id));
    assert_eq!(ledger[0].experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(ledger[0].attribution_correlation_id.as_deref(), Some("run-stream"));
    assert_eq!(ledger[0].tokens_in, 10);
    assert_eq!(ledger[0].tokens_out, 20);

    let prompts = h.prompts().await;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].routed_model, "model-b");
    assert_eq!(prompts[0].experiment_id, Some(id));
    assert_eq!(prompts[0].experiment_variant.as_deref(), Some("candidate"));
    assert_eq!(ledger[0].prompt_id, prompts[0].id);
}

#[tokio::test]
async fn pinned_model_survives_the_alias_moving() {
    let h = build_app(Options::default()).await;
    let id = h.seed_experiment(vec![], false).await;

    let mut moved = HashMap::new();
    moved.insert("planner".to_string(), "mock/model-c".to_string());
    h.router.update_db_aliases(moved);

    let resp = complete(&h, TOKEN_A, None, &chat_body("planner", "run-1")).await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(resp.json::<Value>()["model"], "model-c");

    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{id}:candidate")),
        &chat_body("planner", "run-1"),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert_eq!(resp.json::<Value>()["model"], "model-b");
    assert_eq!(h.calls(), vec!["model-c", "model-b"]);
}

#[tokio::test]
async fn pool_name_under_a_binding_is_a_400() {
    let h = build_app(Options {
        pool: true,
        ..Default::default()
    })
    .await;
    let id = h.seed_experiment(vec![], false).await;

    // Unbound, the pool picks a member.
    let resp = complete(&h, TOKEN_A, None, &chat_body("pool", "run-1")).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());

    // Bound, the overlay does not map `pool`, so the effective name is still
    // the pool and that is refused: an experiment pins one model.
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{id}:control")),
        &chat_body("pool", "run-1"),
    )
    .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    assert!(resp.text().contains("'pool' is a load balancer pool"));
    assert_eq!(h.calls().len(), 1);
}

#[tokio::test]
async fn retaining_binding_writes_the_prompt_row_with_content_when_storage_is_off() {
    let h = build_app(Options {
        store_prompts: false,
        ..Default::default()
    })
    .await;
    let plain = h.seed_experiment(vec![], false).await;
    let retaining = h.seed_experiment(vec![], true).await;

    // Unbound: cost only, no prompt row, as `store_prompts = false` demands.
    let resp = complete(&h, TOKEN_A, None, &chat_body("planner", "run-1")).await;
    assert_eq!(resp.status_code(), 200);
    let stamp = h.wait_for_run(1, "run-1").await;
    assert_eq!(stamp.experiment_id, None);
    assert!(h.prompts().await.is_empty());

    // Bound to a non-retaining experiment: stamped, still no prompt row.
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{plain}:candidate")),
        &chat_body("planner", "run-2"),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    let stamp = h.wait_for_run(1, "run-2").await;
    assert_eq!(stamp.experiment_id, Some(plain));
    assert_eq!(stamp.experiment_variant.as_deref(), Some("candidate"));
    assert!(h.prompts().await.is_empty());

    // Bound to a retaining experiment: the row is written, with content.
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{retaining}:candidate")),
        &chat_body("planner", "run-3"),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    let stamp = h.wait_for_run(1, "run-3").await;
    assert_eq!(stamp.experiment_id, Some(retaining));
    let prompts = h.prompts().await;
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].experiment_id, Some(retaining));
    assert_eq!(prompts[0].attribution_correlation_id.as_deref(), Some("run-3"));
    assert!(prompts[0].messages.contains("plan the week"));
    assert_eq!(prompts[0].response.as_deref(), Some("answer from model-b"));

    // x-no-log is the caller's own request and still wins.
    let resp = h
        .server
        .post("/v1/chat/completions")
        .add_header(bearer(TOKEN_A).0, bearer(TOKEN_A).1)
        .add_header(
            experiment_header(&format!("{retaining}:candidate")).0,
            experiment_header(&format!("{retaining}:candidate")).1,
        )
        .add_header(
            axum::http::HeaderName::from_static("x-no-log"),
            axum::http::HeaderValue::from_static("true"),
        )
        .json(&chat_body("planner", "run-4"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let stamp = h.wait_for_run(1, "run-4").await;
    assert_eq!(stamp.experiment_id, Some(retaining));
    assert_eq!(h.prompts().await.len(), 1);
}

#[tokio::test]
async fn retaining_binding_stores_content_when_content_storage_is_off() {
    let h = build_app(Options {
        store_prompt_content: false,
        ..Default::default()
    })
    .await;
    let plain = h.seed_experiment(vec![], false).await;
    let retaining = h.seed_experiment(vec![], true).await;

    // Unbound: the operator's policy, a redacted row.
    let resp = complete(&h, TOKEN_A, None, &chat_body("planner", "run-1")).await;
    assert_eq!(resp.status_code(), 200);
    h.wait_for_run(1, "run-1").await;
    let row = h.prompt_for_run("run-1").await;
    assert_eq!(row.messages, CONTENT_NOT_STORED);
    assert_eq!(row.response, None);

    // Bound to a non-retaining experiment: stamped, still redacted.
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{plain}:candidate")),
        &chat_body("planner", "run-2"),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    h.wait_for_run(1, "run-2").await;
    let row = h.prompt_for_run("run-2").await;
    assert_eq!(row.experiment_id, Some(plain));
    assert_eq!(row.messages, CONTENT_NOT_STORED);
    assert_eq!(row.response, None);

    // Bound to a retaining experiment: full messages and response.
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{retaining}:candidate")),
        &chat_body("planner", "run-3"),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    h.wait_for_run(1, "run-3").await;
    let row = h.prompt_for_run("run-3").await;
    assert_eq!(row.experiment_id, Some(retaining));
    assert_eq!(row.experiment_variant.as_deref(), Some("candidate"));
    assert!(row.messages.contains("plan the week"), "{}", row.messages);
    assert_eq!(row.response.as_deref(), Some("answer from model-b"));
    assert!(row.latency_ms.is_some());

    // The same on the streaming path, written once `[DONE]` has passed.
    let mut body = chat_body("planner", "run-4");
    body["stream"] = json!(true);
    let resp = complete(&h, TOKEN_A, Some(&format!("{retaining}:candidate")), &body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    assert!(resp.text().contains("data: [DONE]"));
    h.wait_for_run(1, "run-4").await;
    let row = h.prompt_for_run("run-4").await;
    assert_eq!(row.experiment_id, Some(retaining));
    assert!(row.messages.contains("plan the week"), "{}", row.messages);
    assert_eq!(row.response.as_deref(), Some("answer from model-b"));
    assert_eq!(row.prompt_tokens, 10);
    assert_eq!(row.completion_tokens, 20);

    // A streaming request bound to the non-retaining experiment is redacted.
    let mut body = chat_body("planner", "run-5");
    body["stream"] = json!(true);
    let resp = complete(&h, TOKEN_A, Some(&format!("{plain}:candidate")), &body).await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    h.wait_for_run(1, "run-5").await;
    let row = h.prompt_for_run("run-5").await;
    assert_eq!(row.experiment_id, Some(plain));
    assert_eq!(row.messages, CONTENT_NOT_STORED);
    assert_eq!(row.response, None);

    // x-no-log still wins over retention on the streaming path too.
    let mut body = chat_body("planner", "run-6");
    body["stream"] = json!(true);
    let resp = h
        .server
        .post("/v1/chat/completions")
        .add_header(bearer(TOKEN_A).0, bearer(TOKEN_A).1)
        .add_header(
            experiment_header(&format!("{retaining}:candidate")).0,
            experiment_header(&format!("{retaining}:candidate")).1,
        )
        .add_header(
            axum::http::HeaderName::from_static("x-no-log"),
            axum::http::HeaderValue::from_static("true"),
        )
        .json(&body)
        .await;
    assert_eq!(resp.status_code(), 200);
    let stamp = h.wait_for_run(1, "run-6").await;
    assert_eq!(stamp.experiment_id, Some(retaining));
    assert_eq!(h.prompts().await.len(), 5);
}

#[tokio::test]
async fn retaining_binding_never_opens_the_callback_egress() {
    // With storage on, the recorder proves the egress is wired.
    let h = build_app(Options::default()).await;
    let resp = complete(&h, TOKEN_A, None, &chat_body("planner", "run-0")).await;
    assert_eq!(resp.status_code(), 200);
    h.wait_for_run(1, "run-0").await;
    let events = h.wait_for_events(1).await;
    assert_eq!(events[0].output, "answer from model-a");

    // With `store_prompts = false`, a retaining binding writes its prompt row
    // with content and nothing leaves the router.
    let h = build_app(Options {
        store_prompts: false,
        ..Default::default()
    })
    .await;
    let retaining = h.seed_experiment(vec![], true).await;
    let resp = complete(
        &h,
        TOKEN_A,
        Some(&format!("{retaining}:candidate")),
        &chat_body("planner", "run-1"),
    )
    .await;
    assert_eq!(resp.status_code(), 200);
    let stamp = h.wait_for_run(1, "run-1").await;
    assert_eq!(stamp.experiment_id, Some(retaining));
    let row = h.prompt_for_run("run-1").await;
    assert!(row.messages.contains("plan the week"));
    assert_eq!(row.response.as_deref(), Some("answer from model-b"));
    assert!(h.events().is_empty(), "{:?}", h.events().len());
}
