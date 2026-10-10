//! Per-request upstream lifecycle logging through the real completions route:
//! a call the upstream never answers shows a start and no headers, and a
//! caller that gives up is logged as `dropped`; streamed and non-streamed
//! calls that complete log their whole lifecycle with the caller's
//! correlation id.

mod common;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState};
use modelrouter::config::schema::{CacheConfig, LogFormat, Settings};
use modelrouter::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry, registry::ProviderRegistry, search_registry::SearchRegistry,
};
use modelrouter::router::{
    cache::ResponseCache, complexity::ComplexityRouter, cost::CostCalculator, engine::RequestRouter,
    fallback::FallbackChain, policy::PolicyEngine,
};
use serde_json::{json, Value};
use tracing_subscriber::layer::SubscriberExt;

/// Answers normally, or (`silent`) accepts the call and never answers.
struct Upstream {
    silent: bool,
}

#[async_trait::async_trait]
impl ProviderAdapter for Upstream {
    async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        if self.silent {
            std::future::pending::<()>().await;
        }
        Ok(CompletionResult {
            content: "hello".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
            finish_reason: "stop".to_string(),
            ..Default::default()
        })
    }

    async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        if self.silent {
            std::future::pending::<()>().await;
        }
        let data = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
        Ok(Box::pin(futures::stream::once(async move { Ok::<bytes::Bytes, anyhow::Error>(bytes::Bytes::from(data)) })))
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    /// The lifecycle events logged so far.
    fn lifecycle(&self) -> Vec<Value> {
        let text = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["message"].as_str().is_some_and(|m| m.starts_with("upstream ")))
            .collect()
    }
}

async fn app(silent: bool) -> TestServer {
    let db = common::in_memory_db().await;
    common::create_user(&db, "test-user", "test-token").await;
    let settings = Arc::new(Settings::default());
    let db: Arc<dyn modelrouter::api::app::DatabaseProvider> = Arc::new(db);
    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new_with_config(&settings.pricing)),
        provider_registry: Arc::new(ProviderRegistry::new_with_mock(Upstream { silent })),
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(FallbackChain::new(HashMap::new())),
        complexity_router: Arc::new(ComplexityRouter::new(None)),
        response_cache: Arc::new(ResponseCache::new(&CacheConfig::default())),
        embedding_registry: Arc::new(EmbeddingRegistry::new_with_mock(common::MockEmbeddingAdapter {
            embedding: vec![0.1_f32],
        })),
        search_registry: Arc::new(SearchRegistry::new_with_mock(common::MockSearchAdapter { results: vec![] })),
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
        experiments: Arc::new(modelrouter::router::experiments::ExperimentRegistry::default()),
    };
    TestServer::new(build_router(state)).unwrap()
}

fn request(stream: bool) -> Value {
    json!({
        "model": "mock-model",
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
        "attribution": {"correlation_id": "run-7"},
    })
}

fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let out = Captured::default();
    let subscriber = tracing_subscriber::registry().with(modelrouter::logging::fmt_layer(LogFormat::Json, out.clone()));
    (out, tracing::subscriber::set_default(subscriber))
}

fn messages(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["message"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn a_streamed_call_logs_start_headers_and_finish_with_the_callers_correlation_id() {
    let (out, _guard) = capture();
    let server = app(false).await;
    let resp = server.post("/v1/chat/completions").add_header(axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static("Bearer test-token")).json(&request(true)).await;
    assert_eq!(resp.status_code(), 200);
    let events = out.lifecycle();
    assert_eq!(
        messages(&events),
        ["upstream request started", "upstream response headers received", "upstream request finished"]
    );
    assert!(events.iter().all(|e| e["correlation_id"] == "run-7"), "{events:#?}");
    assert!(events[0]["request_id"].as_str().unwrap().starts_with("chatcmpl-mr-"));
    assert!(events.iter().all(|e| e["request_id"] == events[0]["request_id"]));
    assert_eq!(events[2]["outcome"], "completed");
}

#[tokio::test]
async fn a_non_streamed_call_logs_start_and_finish() {
    let (out, _guard) = capture();
    let server = app(false).await;
    let resp = server.post("/v1/chat/completions").add_header(axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static("Bearer test-token")).json(&request(false)).await;
    assert_eq!(resp.status_code(), 200);
    let events = out.lifecycle();
    assert_eq!(messages(&events), ["upstream request started", "upstream request finished"]);
    assert_eq!(events[1]["outcome"], "completed");
    assert_eq!(events[1]["streaming"], false);
    assert_eq!(events[1]["correlation_id"], "run-7");
}

#[tokio::test]
async fn an_upstream_that_never_answers_shows_a_start_without_headers_then_dropped_when_the_caller_gives_up() {
    let (out, _guard) = capture();
    let server = app(true).await;
    let call = server.post("/v1/chat/completions").add_header(axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static("Bearer test-token")).json(&request(true));
    let gave_up = tokio::time::timeout(Duration::from_millis(300), async move { call.await }).await;
    assert!(gave_up.is_err(), "the silent upstream answered");
    let events = out.lifecycle();
    assert_eq!(messages(&events), ["upstream request started", "upstream request finished"], "{events:#?}");
    assert_eq!(events[1]["outcome"], "dropped");
    assert_eq!(events[1]["headers_ms"], -1);
    assert!(events[1]["latency_ms"].as_u64().unwrap() >= 200);
}
