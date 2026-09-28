mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::config::Settings;
use modelrouter::api::auth::hash_token;
use modelrouter::db::models::{NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::guardrails::{Guardrail, GuardrailChain, GuardrailContext, GuardrailDecision};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> TestServer {
    let db = common::in_memory_db().await;

    // Create a test user
    db.create(NewUser {
        name: "test-user".to_string(),
        email: None,
    })
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
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "Hello!".to_string(),
    }));

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
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));

    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
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

#[tokio::test]
async fn unauthenticated_request_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn valid_request_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["choices"][0]["message"]["content"], "Hello!");
}

struct BlockAllGuardrail;

#[async_trait::async_trait]
impl Guardrail for BlockAllGuardrail {
    fn name(&self) -> &str { "block-all" }
    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Block { reason: "blocked by test guardrail".to_string() }
    }
    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Allow
    }
}

async fn test_app_with_blocking_guardrail() -> TestServer {
    let db = common::in_memory_db().await;
    db.create(NewUser {
        name: "test-user".to_string(),
        email: None,
    })
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
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "Hello!".to_string(),
    }));
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
    let guardrails = Arc::new(GuardrailChain::new(vec![
        (Box::new(BlockAllGuardrail) as Box<dyn Guardrail>, false),
    ]));
    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
        storage: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
        prompt_db: db.clone(),
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(std::collections::HashMap::new())),
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails,
        oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn blocking_guardrail_returns_400() {
    let server = test_app_with_blocking_guardrail().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}

// ── Accounting: the model that answered, and the usage it reported ───────────
//
// Cost, prompt and ledger rows must name the provider and model that actually
// produced the response — after fallback, not the one first resolved — and a
// streamed response must carry the provider's own usage figures when the
// stream has them, flagged as estimated otherwise, and exist even when the
// stream ends early.

mod accounting {
    use super::common;
    use axum_test::{TestServer, TestServerConfig, Transport};
    use futures::StreamExt;
    use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
    use modelrouter::api::auth::hash_token;
    use modelrouter::config::schema::{PricingEntry, Settings, StorageConfig};
    use modelrouter::db::models::{NewApiKey, NewUser};
    use modelrouter::db::repositories::api_keys::ApiKeyRepository;
    use modelrouter::db::repositories::failures::FailureRepository;
    use modelrouter::db::repositories::prompts::PromptRepository;
    use modelrouter::db::repositories::users::UserRepository;
    use modelrouter::providers::adapter::{
        CompletionResult, NormalizedRequest, ProviderAdapter, SseStream,
    };
    use modelrouter::providers::anthropic::AnthropicSseTranslator;
    use modelrouter::providers::registry::ProviderRegistry;
    use modelrouter::router::{
        cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain,
        policy::PolicyEngine,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;


    /// One scripted stream chunk: bytes to yield, an error to raise, or a
    /// stall that never resolves (so a client can abandon the response).
    #[derive(Clone)]
    enum Chunk {
        Data(String),
        Error(String),
        Hang,
    }

    /// An adapter whose non-streaming calls fail a set number of times before
    /// answering, and whose stream is a script. `new_with_mock` hands the same
    /// adapter out for every provider name, so a fallback chain that crosses
    /// providers still lands here.
    pub(crate) struct ScriptedAdapter {
        fail_first: AtomicUsize,
        chunks: Vec<Chunk>,
    }

    impl ScriptedAdapter {
        pub(crate) fn streaming(chunks: Vec<Chunk>) -> Self {
            Self {
                fail_first: AtomicUsize::new(0),
                chunks,
            }
        }

        pub(crate) fn failing_first(n: usize) -> Self {
            Self {
                fail_first: AtomicUsize::new(n),
                chunks: vec![],
            }
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for ScriptedAdapter {
        async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
            let remaining = self.fail_first.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_first.store(remaining - 1, Ordering::SeqCst);
                // No status code in the message: classified as not retryable,
                // so the router goes straight to the fallback chain.
                anyhow::bail!("primary provider unavailable");
            }
            Ok(CompletionResult {
                content: "answered".to_string(),
                prompt_tokens: 100,
                completion_tokens: 50,
                finish_reason: "stop".to_string(),
                // A recognisable TTFT, so tests can check the handler carried
                // the adapter's measurement into the prompt row.
                ttft_ms: Some(42),
                tool_calls: None,
                ..Default::default()
            })
        }

        async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
            let chunks = self.chunks.clone();
            let stream = futures::stream::iter(chunks).flat_map(|c| match c {
                Chunk::Data(s) => futures::stream::once(async move {
                    Ok::<bytes::Bytes, anyhow::Error>(bytes::Bytes::from(s))
                })
                .boxed(),
                Chunk::Error(e) => {
                    futures::stream::once(async move { Err(anyhow::anyhow!("{e}")) }).boxed()
                }
                Chunk::Hang => futures::stream::pending().boxed(),
            });
            Ok(Box::pin(stream))
        }
    }

    fn sse(json: serde_json::Value) -> String {
        format!("data: {json}\n\n")
    }

    fn delta(text: &str) -> String {
        sse(json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}))
    }

    /// Prices: the primary model costs ten times the fallback, so a row priced
    /// at the wrong rate is unmistakable.
    fn pricing() -> Vec<PricingEntry> {
        vec![
            PricingEntry {
                model: "big-model".to_string(),
                input_per_million: 1000.0,
                output_per_million: 1000.0,
                ..Default::default()
            },
            PricingEntry {
                model: "mini-model".to_string(),
                input_per_million: 100.0,
                output_per_million: 100.0,
                ..Default::default()
            },
        ]
    }

    pub(crate) async fn build_app(
        adapter: ScriptedAdapter,
        chains: HashMap<String, Vec<String>>,
        real_port: bool,
    ) -> (TestServer, Arc<dyn DatabaseProvider>) {
        let db = common::in_memory_db().await;
        UserRepository::create(&db, NewUser { name: "test-user".to_string(), email: None })
            .await
            .unwrap();
        let user = UserRepository::find_by_name(&db, "test-user").await.unwrap().unwrap();
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

        let settings = Arc::new(Settings {
            pricing: pricing(),
            ..Default::default()
        });
        let db: Arc<dyn DatabaseProvider> = Arc::new(db);

        let state = AppState {
            live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
            // Content on, so the prompt row's response text can be checked.
            storage: Arc::new(arc_swap::ArcSwap::from_pointee(StorageConfig {
                store_prompt_content: true,
                ..Default::default()
            })),
            prompt_db: db.clone(),
            settings: settings.clone(),
            db: db.clone(),
            pool: None,
            router: Arc::new(RequestRouter::new(settings.clone())),
            cost_calc: Arc::new(CostCalculator::new_with_config(&settings.pricing)),
            provider_registry: Arc::new(ProviderRegistry::new_with_mock(adapter)),
            policy: Arc::new(PolicyEngine::new(db.clone())),
            fallback: Arc::new(FallbackChain::new(chains)),
            complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
            response_cache: Arc::new(modelrouter::router::cache::ResponseCache::new(
                &modelrouter::config::schema::CacheConfig::default(),
            )),
            embedding_registry: Arc::new(
                modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
                    common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
                ),
            ),
            search_registry: Arc::new(modelrouter::providers::search_registry::SearchRegistry::new(
                HashMap::new(),
            )),
            load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
                HashMap::new(),
            )),
            concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
            circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
            ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
            session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
            session_affinity: Arc::new(modelrouter::router::session_affinity::SessionAffinityMap::new(1800)),
            app_metrics: None,
            callbacks: Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
            guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
            oidc_state: Arc::new(modelrouter::api::admin::oidc::OidcStateStore::new()),
            experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
        };
        let server = if real_port {
            // A real socket, so a test can read the body chunk by chunk and
            // abandon it half way — the mock transport collects whole bodies.
            let config = TestServerConfig {
                transport: Some(Transport::HttpRandomPort),
                ..Default::default()
            };
            TestServer::new_with_config(build_router(state), config).unwrap()
        } else {
            TestServer::new(build_router(state)).unwrap()
        };
        (server, db)
    }

    pub(crate) fn bearer() -> (axum::http::HeaderName, axum::http::HeaderValue) {
        (
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
    }

    pub(crate) fn request_body(stream: bool) -> serde_json::Value {
        json!({
            "model": "primary/big-model",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": stream,
        })
    }


    fn close_to(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[tokio::test]
    async fn fallback_answer_is_recorded_under_the_fallback_model_and_price() {
        let chains = HashMap::from([(
            "big-model".to_string(),
            vec!["backup/mini-model".to_string()],
        )]);
        let (server, db) = build_app(ScriptedAdapter::failing_first(1), chains, false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(false))
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["model"], "mini-model", "the response names the model that answered");

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].model, "mini-model");
        assert_eq!(ledger[0].provider, "backup");
        assert_eq!(ledger[0].tokens_in, 100);
        assert_eq!(ledger[0].tokens_out, 50);
        // 150 tokens at the fallback's $100/M, not the primary's $1000/M.
        assert!(close_to(ledger[0].cost_usd, 0.015), "cost was {}", ledger[0].cost_usd);
        assert!(!ledger[0].tokens_estimated);

        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].request_model, "primary/big-model");
        assert_eq!(prompts[0].routed_model, "mini-model");
        assert_eq!(prompts[0].provider, "backup");
        assert!(close_to(prompts[0].cost_usd, 0.015));
        // The adapter's TTFT measurement lands on the row.
        assert_eq!(prompts[0].ttft_ms, Some(42));
        // Primary failed once, the fallback answered: two provider calls.
        assert_eq!(prompts[0].attempts, Some(2));
    }

    #[tokio::test]
    async fn without_a_fallback_the_resolved_model_is_recorded() {
        let (server, db) = build_app(ScriptedAdapter::failing_first(0), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(false))
            .await;
        assert_eq!(resp.status_code(), 200);
        assert_eq!(resp.json::<serde_json::Value>()["model"], "big-model");

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger[0].model, "big-model");
        assert_eq!(ledger[0].provider, "primary");
        assert!(close_to(ledger[0].cost_usd, 0.15), "cost was {}", ledger[0].cost_usd);
        assert!(!ledger[0].tokens_estimated);

        // First-try success is exactly one attempt.
        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].attempts, Some(1));
    }

    #[tokio::test]
    async fn streamed_usage_from_the_provider_is_recorded_as_measured() {
        let chunks = vec![
            Chunk::Data(delta("Hello")),
            Chunk::Data(sse(json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}))),
            Chunk::Data(format!(
                "{}data: [DONE]\n\n",
                sse(json!({"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":6,"total_tokens":46}}))
            )),
        ];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(true))
            .await;
        assert_eq!(resp.status_code(), 200);
        assert!(resp.text().contains("Hello"));

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger[0].model, "big-model");
        assert_eq!(ledger[0].provider, "primary");
        assert_eq!(ledger[0].tokens_in, 40);
        assert_eq!(ledger[0].tokens_out, 6);
        assert!(!ledger[0].tokens_estimated);
        assert!(close_to(ledger[0].cost_usd, 0.046), "cost was {}", ledger[0].cost_usd);

        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].prompt_tokens, 40);
        assert_eq!(prompts[0].completion_tokens, 6);
        assert_eq!(prompts[0].finish_reason.as_deref(), Some("length"));
        assert_eq!(prompts[0].response.as_deref(), Some("Hello"));
        // Streamed responses record time-to-first-chunk as TTFT and a single
        // dispatch as one attempt (the streaming path has no retry loop).
        assert!(prompts[0].ttft_ms.is_some(), "streamed row should carry a TTFT");
        assert_eq!(prompts[0].attempts, Some(1));
    }

    #[tokio::test]
    async fn stream_without_usage_records_the_estimate_and_flags_it() {
        let chunks = vec![Chunk::Data(format!("{}data: [DONE]\n\n", delta("Hello world!")))];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(true))
            .await;
        assert_eq!(resp.status_code(), 200);

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert!(ledger[0].tokens_estimated);
        // Twelve characters of output at four characters per token.
        assert_eq!(ledger[0].tokens_out, 3);
        assert!(ledger[0].tokens_in > 0, "the prompt estimate comes from the messages");
        let expected = (ledger[0].tokens_in + ledger[0].tokens_out) as f64 * 1000.0 / 1_000_000.0;
        assert!(close_to(ledger[0].cost_usd, expected));

        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].finish_reason.as_deref(), Some("stop"));
    }

    /// The `usage` object of the router's cost chunk: the last `data:` event
    /// before `[DONE]`.
    fn cost_chunk_usage(body: &str) -> serde_json::Value {
        let events: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        assert_eq!(events.last().copied(), Some("[DONE]"), "stream must still end with [DONE]");
        let cost_event: serde_json::Value =
            serde_json::from_str(events[events.len() - 2]).expect("cost chunk is JSON");
        assert_eq!(cost_event["object"], "chat.completion.chunk");
        cost_event["usage"].clone()
    }

    /// The `x_router` object of the router's cost chunk.
    fn cost_chunk_meta(body: &str) -> serde_json::Value {
        let events: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        let cost_event: serde_json::Value =
            serde_json::from_str(events[events.len() - 2]).expect("cost chunk is JSON");
        cost_event["x_router"].clone()
    }

    #[tokio::test]
    async fn non_stream_response_reports_exactly_the_ledger_cost() {
        let (server, db) = build_app(ScriptedAdapter::failing_first(0), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(false))
            .await;
        assert_eq!(resp.status_code(), 200);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["model"], "big-model", "the routed model is returned");

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        let returned = body["usage"]["cost_usd"].as_f64().expect("usage.cost_usd present");
        assert_eq!(returned, ledger[0].cost_usd, "response cost must be the ledger value");
        assert!(close_to(returned, 0.15));

        let meta = &body["x_router"];
        assert_eq!(meta["requested_model"], "primary/big-model");
        assert_eq!(meta["model"], ledger[0].model.as_str());
        assert_eq!(meta["provider"], ledger[0].provider.as_str());
        assert_eq!(meta["cost"]["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
        assert_eq!(meta["cost"]["cache_hit"], false);
        assert_eq!(meta["tokens"]["prompt"].as_i64(), Some(ledger[0].tokens_in));
        assert_eq!(meta["tokens"]["completion"].as_i64(), Some(ledger[0].tokens_out));
        assert_eq!(meta["tokens"]["estimated"], false);
        assert_eq!(meta["timing"]["attempts"], 1);
        assert_eq!(meta["timing"]["fallbacks"], 0);
        assert!(meta["timing"]["total_ms"].as_i64().unwrap() >= meta["timing"]["latency_ms"].as_i64().unwrap());
    }

    #[tokio::test]
    async fn fallback_response_reports_the_fallback_ledger_cost() {
        let chains = HashMap::from([(
            "big-model".to_string(),
            vec!["backup/mini-model".to_string()],
        )]);
        let (server, db) = build_app(ScriptedAdapter::failing_first(1), chains, false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(false))
            .await;
        let body: serde_json::Value = resp.json();
        assert_eq!(body["model"], "mini-model");
        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(body["usage"]["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
        assert!(close_to(ledger[0].cost_usd, 0.015));

        let meta = &body["x_router"];
        assert_eq!(meta["model"], ledger[0].model.as_str(), "the fallback model is named");
        assert_eq!(meta["provider"], ledger[0].provider.as_str());
        assert_eq!(meta["cost"]["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
        assert_eq!(meta["timing"]["attempts"], 2, "the failed primary call counts");
        assert_eq!(meta["timing"]["fallbacks"], 1);
    }

    #[tokio::test]
    async fn streamed_response_ends_with_exactly_the_ledger_cost() {
        let chunks = vec![
            Chunk::Data(delta("Hello")),
            Chunk::Data(sse(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))),
            Chunk::Data(format!(
                "{}data: [DONE]\n\n",
                sse(json!({"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":6,"total_tokens":46}}))
            )),
        ];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(true))
            .await;
        assert_eq!(resp.status_code(), 200);
        let text = resp.text();
        let usage = cost_chunk_usage(&text);

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        let returned = usage["cost_usd"].as_f64().expect("usage.cost_usd present");
        assert_eq!(returned, ledger[0].cost_usd, "stream cost must be the ledger value");
        assert!(close_to(returned, 0.046));
        assert_eq!(usage["prompt_tokens"], 40);
        assert_eq!(usage["completion_tokens"], 6);
        assert!(usage.get("tokens_estimated").is_none());
        let meta = cost_chunk_meta(&text);
        assert_eq!(meta["model"], ledger[0].model.as_str());
        assert_eq!(meta["provider"], ledger[0].provider.as_str());
        assert_eq!(meta["cost"]["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
        assert_eq!(meta["tokens"]["prompt"].as_i64(), Some(ledger[0].tokens_in));
        assert_eq!(meta["tokens"]["completion"].as_i64(), Some(ledger[0].tokens_out));
        assert_eq!(meta["timing"]["attempts"], 1);
        // The provider's own chunks still pass through untouched.
        assert!(text.contains("Hello"));
    }

    #[tokio::test]
    async fn streamed_estimate_is_returned_with_its_flag() {
        let chunks = vec![Chunk::Data(format!("{}data: [DONE]\n\n", delta("Hello world!")))];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(true))
            .await;
        let usage = cost_chunk_usage(&resp.text());

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert!(ledger[0].tokens_estimated);
        assert_eq!(usage["tokens_estimated"], true);
        assert_eq!(usage["cost_usd"].as_f64(), Some(ledger[0].cost_usd));
        assert_eq!(usage["completion_tokens"].as_i64(), Some(ledger[0].tokens_out));
    }

    #[tokio::test]
    async fn anthropic_stream_usage_reaches_the_ledger() {
        // Anthropic-shaped events run through the real translator, the way the
        // adapter feeds them; the ledger sees only the translated chunks.
        let raw = [
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":40,"cache_read_input_tokens":10,"output_tokens":1}}}"#,
            r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            r#"data: {"type":"message_stop"}"#,
        ];
        let mut translator = AnthropicSseTranslator::new();
        let chunks = raw
            .iter()
            .filter_map(|line| translator.translate_line(line))
            .map(|b| Chunk::Data(String::from_utf8(b.to_vec()).unwrap()))
            .collect();
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(true))
            .await;
        assert_eq!(resp.status_code(), 200);

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger[0].tokens_in, 50, "input plus cache-read tokens");
        assert_eq!(ledger[0].tokens_out, 7);
        assert!(!ledger[0].tokens_estimated);
        // 40 uncached input + 7 output at $1000/M, 10 cache reads at 10% of input.
        assert!(close_to(ledger[0].cost_usd, 0.048), "cost was {}", ledger[0].cost_usd);

        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].cache_read_tokens, 10);
        assert_eq!(prompts[0].response.as_deref(), Some("Hi"));
    }

    #[tokio::test]
    async fn stream_that_errors_mid_way_still_writes_a_ledger_row_and_a_failure() {
        let chunks = vec![
            Chunk::Data(delta("partial answer")),
            Chunk::Error("upstream connection reset".to_string()),
        ];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), true).await;

        let url = format!("{}v1/chat/completions", server.server_address().unwrap());
        // The client sees the break either as a body error or, when hyper
        // resets the connection before the headers are flushed, as a failed
        // send. Both are the broken stream; the ledger is the point.
        if let Ok(resp) = reqwest::Client::new()
            .post(&url)
            .bearer_auth("test-token")
            .json(&request_body(true))
            .send()
            .await
        {
            assert_eq!(resp.status(), 200);
            let mut body = resp.bytes_stream();
            while body.next().await.is_some() {}
        }

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger.len(), 1);
        assert!(ledger[0].tokens_estimated);
        // "partial answer" is fourteen characters: three estimated tokens.
        assert_eq!(ledger[0].tokens_out, 3);

        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].finish_reason.as_deref(), Some("error"));
        assert_eq!(prompts[0].response.as_deref(), Some("partial answer"));

        let failures = FailureRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].stage, "provider");
        assert_eq!(failures[0].routed_model.as_deref(), Some("big-model"));
        assert!(failures[0].error_message.contains("upstream connection reset"));
    }

    #[tokio::test]
    async fn stream_abandoned_by_the_client_still_writes_a_ledger_row() {
        let chunks = vec![Chunk::Data(delta("first words")), Chunk::Hang];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), true).await;

        let url = format!("{}v1/chat/completions", server.server_address().unwrap());
        let resp = reqwest::Client::new()
            .post(&url)
            .bearer_auth("test-token")
            .json(&request_body(true))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let mut body = resp.bytes_stream();
        let first = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("first words"));
        // Walk away mid-stream: the connection closes and the router's body is
        // dropped without ever seeing [DONE].
        drop(body);

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert_eq!(ledger.len(), 1);
        assert!(ledger[0].tokens_estimated);
        assert_eq!(ledger[0].tokens_out, 2);

        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].finish_reason.as_deref(), Some("aborted"));
        assert_eq!(prompts[0].response.as_deref(), Some("first words"));
    }

    #[tokio::test]
    async fn stream_that_ends_without_a_terminal_chunk_still_writes_a_ledger_row() {
        let chunks = vec![Chunk::Data(delta("cut off"))];
        let (server, db) = build_app(ScriptedAdapter::streaming(chunks), HashMap::new(), false).await;

        let resp = server
            .post("/v1/chat/completions")
            .add_header(bearer().0, bearer().1)
            .json(&request_body(true))
            .await;
        assert_eq!(resp.status_code(), 200);

        let ledger = common::wait_for_ledger_rows(&*db, 1).await;
        assert!(ledger[0].tokens_estimated);
        let prompts = PromptRepository::list(&*db, 10, 0).await.unwrap();
        assert_eq!(prompts[0].finish_reason.as_deref(), Some("aborted"));
    }
}

#[tokio::test]
async fn tools_field_with_non_empty_array_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"type": "function", "function": {"name": "test"}}]
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("tools"), "Error message should mention 'tools' field, got: {}", message);
}

#[tokio::test]
async fn tool_choice_field_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tool_choice": "auto"
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("tool_choice"), "Error message should mention 'tool_choice' field, got: {}", message);
}

#[tokio::test]
async fn empty_tools_array_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "tools": []
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn request_without_tools_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn tools_field_with_streaming_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true,
            "tools": [{"type": "function", "function": {"name": "test"}}]
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: serde_json::Value = resp.json();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("tools"), "Error message should mention 'tools' field, got: {}", message);
}

// ── Fallback policy re-checks (issue #72) ────────────────────────────────────

#[tokio::test]
async fn fallback_denied_by_user_model_allow_list_fails_request() {
    use modelrouter::db::models::{NewBudgetRule, BudgetScope};
    use modelrouter::db::repositories::budgets::BudgetRepository;

    // Build app with primary→fallback chain; primary provider fails
    let chains = std::collections::HashMap::from([(
        "big-model".to_string(),
        vec!["backup/mini-model".to_string()],
    )]);
    let (server, db) = accounting::build_app(
        accounting::ScriptedAdapter::failing_first(1),
        chains,
        false,
    ).await;

    // Create a budget rule that only allows the primary model (not the fallback)
    // The primary model "big-model" is resolved to "primary/big-model" by the router,
    // but the policy checks against canonical "big-model" after resolution.
    let user = UserRepository::find_by_name(&*db, "test-user").await.unwrap().unwrap();
    BudgetRepository::create(
        &*db,
        NewBudgetRule {
            user_id: Some(user.id),
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            // Allow the primary model (as sent in the request) but not the fallback
            // The primary policy check sees "primary/big-model" (before resolution),
            // the fallback re-check sees "mini-model" (after resolution of "backup/mini-model")
            model_allow: vec!["primary/big-model".to_string()],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    // Primary provider fails, fallback is denied by policy → request fails
    let resp = server
        .post("/v1/chat/completions")
        .add_header(accounting::bearer().0, accounting::bearer().1)
        .json(&accounting::request_body(false))
        .await;

    // The request should fail (fallback was skipped by policy)
    assert_eq!(resp.status_code(), 502, "fallback denied by policy should fail the request");

    // No ledger row: neither the primary (failed) nor the fallback (denied) served
    let ledger = common::wait_for_ledger_rows(&*db, 0).await;
    assert_eq!(ledger.len(), 0, "no cost should be recorded when fallback is denied");
}

#[tokio::test]
async fn fallback_permitted_by_model_allow_list_serves_request() {
    use modelrouter::db::models::{NewBudgetRule, BudgetScope};
    use modelrouter::db::repositories::budgets::BudgetRepository;

    // Build app with primary→fallback chain; primary provider fails
    let chains = std::collections::HashMap::from([(
        "big-model".to_string(),
        vec!["backup/mini-model".to_string()],
    )]);
    let (server, db) = accounting::build_app(
        accounting::ScriptedAdapter::failing_first(1),
        chains,
        false,
    ).await;

    // Create a budget rule that allows both models
    let user = UserRepository::find_by_name(&*db, "test-user").await.unwrap().unwrap();
    BudgetRepository::create(
        &*db,
        NewBudgetRule {
            user_id: Some(user.id),
            group_name: None,
            api_key_id: None,
            tag: None,
            project: None,
            window: "monthly".to_string(),
            limit_usd: None,
            limit_tokens: None,
            rate_rpm: None,
            max_concurrent: None,
            // Allow both the primary (as sent) and the fallback (after resolution)
            model_allow: vec!["primary/big-model".to_string(), "mini-model".to_string()],
            model_deny: vec![],
            window_start: None,
            window_end: None,
        },
    )
    .await
    .unwrap();

    // Primary fails, fallback is permitted by policy → fallback serves
    let resp = server
        .post("/v1/chat/completions")
        .add_header(accounting::bearer().0, accounting::bearer().1)
        .json(&accounting::request_body(false))
        .await;

    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["model"], "mini-model");

    // Ledger row records the fallback model
    let ledger = common::wait_for_ledger_rows(&*db, 1).await;
    assert_eq!(ledger[0].model, "mini-model");
}
