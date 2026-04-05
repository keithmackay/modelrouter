#![cfg(feature = "otel")]

mod common;

use modelrouter::telemetry::sampler::SmartSampler;
use opentelemetry::trace::TraceId;
use opentelemetry_sdk::trace::ShouldSample;

fn make_trace_id(low_bytes: u64) -> TraceId {
    let mut bytes = [0u8; 16];
    bytes[8..].copy_from_slice(&low_bytes.to_be_bytes());
    TraceId::from_bytes(bytes)
}

// ── Sampler unit tests (spec 9.1) ──────────────────────────────────────

#[test]
fn sampler_always_records_force_sample() {
    use opentelemetry::{Key, KeyValue, Value};
    let sampler = SmartSampler::new(0.0); // ratio 0 — would DROP without force_sample
    let attrs = vec![KeyValue::new(
        Key::new("modelrouter.force_sample"),
        Value::Bool(true),
    )];
    let result = sampler.should_sample(
        None,
        make_trace_id(12345),
        "test",
        &opentelemetry::trace::SpanKind::Server,
        &attrs,
        &[],
    );
    assert_eq!(
        result.decision,
        opentelemetry::trace::SamplingDecision::RecordAndSample
    );
}

#[test]
fn sampler_ratio_zero_drops_without_force_sample() {
    let sampler = SmartSampler::new(0.0);
    let result = sampler.should_sample(
        None,
        make_trace_id(99999),
        "test",
        &opentelemetry::trace::SpanKind::Server,
        &[],
        &[],
    );
    assert_eq!(
        result.decision,
        opentelemetry::trace::SamplingDecision::Drop
    );
}

#[test]
fn sampler_ratio_one_always_records() {
    let sampler = SmartSampler::new(1.0);
    let result = sampler.should_sample(
        None,
        make_trace_id(42),
        "test",
        &opentelemetry::trace::SpanKind::Server,
        &[],
        &[],
    );
    assert_eq!(
        result.decision,
        opentelemetry::trace::SamplingDecision::RecordAndSample
    );
}

#[test]
fn sampler_propagates_sampled_parent() {
    use opentelemetry::{
        trace::{SpanContext, SpanId, TraceFlags, TraceState},
        Context,
    };
    use opentelemetry::trace::TraceContextExt;

    let sampler = SmartSampler::new(0.0); // would DROP without parent

    // Build a parent context with is_sampled = true
    let parent_sc = SpanContext::new(
        make_trace_id(1),
        SpanId::from_bytes([1, 0, 0, 0, 0, 0, 0, 0]),
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    );
    let parent_ctx = Context::current().with_remote_span_context(parent_sc);

    let result = sampler.should_sample(
        Some(&parent_ctx),
        make_trace_id(1),
        "test",
        &opentelemetry::trace::SpanKind::Server,
        &[],
        &[],
    );
    assert_eq!(
        result.decision,
        opentelemetry::trace::SamplingDecision::RecordAndSample
    );
}

// ── Metrics recording (spec 9.2) ───────────────────────────────────────
//
// Both tests share a single global SdkMeterProvider+InMemoryMetricExporter
// because the metrics module uses OnceLock<Instruments> — only the first
// call to init_instruments() takes effect.
//
// PeriodicReaderWithOwnThread is used instead of PeriodicReader so that
// force_flush() works synchronously without a tokio runtime.

use opentelemetry_sdk::metrics::{PeriodicReaderWithOwnThread, SdkMeterProvider};
use opentelemetry_sdk::testing::metrics::InMemoryMetricExporter;
use opentelemetry::metrics::MeterProvider;
use std::sync::OnceLock;

struct MetricsTestState {
    exporter: InMemoryMetricExporter,
    provider: SdkMeterProvider,
}

static METRICS_TEST_STATE: OnceLock<MetricsTestState> = OnceLock::new();

fn metrics_test_state() -> &'static MetricsTestState {
    METRICS_TEST_STATE.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReaderWithOwnThread::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .build();
        modelrouter::telemetry::metrics::init_instruments(
            provider.meter("test"),
        );
        MetricsTestState { exporter, provider }
    })
}

#[test]
fn metrics_requests_total_increments() {
    let state = metrics_test_state();
    state.exporter.reset();

    modelrouter::telemetry::metrics::record_request("gpt-4o", "openai", "ok");
    modelrouter::telemetry::metrics::record_request("gpt-4o", "openai", "ok");

    state.provider.force_flush().unwrap();

    let metrics = state.exporter.get_finished_metrics().unwrap();
    // Verify the metric exists — .expect() panics if absent
    let _req_metric = metrics.iter()
        .flat_map(|rm| &rm.scope_metrics)
        .flat_map(|sm| &sm.metrics)
        .find(|m| m.name == "modelrouter.requests.total")
        .expect("requests.total metric not found");
}

#[test]
fn metrics_policy_denied_increments_with_reason() {
    let state = metrics_test_state();
    state.exporter.reset();

    modelrouter::telemetry::metrics::record_policy_denied("budget");

    state.provider.force_flush().unwrap();

    let metrics = state.exporter.get_finished_metrics().unwrap();
    let denied = metrics.iter()
        .flat_map(|rm| &rm.scope_metrics)
        .flat_map(|sm| &sm.metrics)
        .find(|m| m.name == "modelrouter.policy.denied");
    assert!(denied.is_some(), "policy.denied metric not found");
}

// ── Init / shutdown (spec 9.3) ─────────────────────────────────────────

#[test]
#[serial_test::serial]
fn telemetry_init_and_shutdown_does_not_panic() {
    use modelrouter::config::schema::TelemetryConfig;
    use modelrouter::telemetry::init_telemetry;

    let config = TelemetryConfig {
        enabled: true,
        endpoint: "http://127.0.0.1:4317".to_string(), // nothing listening — should not block
        service_name: "test".to_string(),
        sample_ratio: 1.0,
        slow_threshold_ms: 5000,
        batch_queue_size: 4,
        batch_scheduled_delay_ms: 100,
        batch_max_export_size: 2,
    };

    // init_telemetry must not panic even when the collector endpoint is unreachable.
    // Use a multi-thread runtime so the background batch processor tasks can run
    // without deadlocking during shutdown (which is called on guard Drop).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let guard = rt.block_on(async { init_telemetry(&config) });
    assert!(guard.is_ok(), "init_telemetry returned error: {:?}", guard.err());

    // Drop the guard (flushes pipelines) inside the runtime so spawned tasks can run.
    drop(guard);
    // Give background tasks a moment to complete (connection refused is fast)
    rt.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(200)).await });
}

// ── Span attribute coverage (spec 9.4) ────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn completions_span_has_required_attributes() {
    use axum_test::TestServer;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use opentelemetry_sdk::Resource;
    use opentelemetry::KeyValue;
    use tracing_subscriber::prelude::*;
    use std::sync::Arc;

    // Set up an in-memory exporter
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_resource(Resource::new(vec![
            KeyValue::new("service.name", "test"),
        ]))
        .with_span_processor(SimpleSpanProcessor::new(Box::new(exporter.clone())))
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());

    // Build a test tracer subscriber layer
    let tracer = provider.tracer("test");
    let otel_layer = tracing_opentelemetry::OpenTelemetryLayer::new(tracer);
    let subscriber = tracing_subscriber::registry()
        .with(otel_layer);
    // Install for this test scope
    let _guard = tracing::subscriber::set_default(subscriber);

    // Build test AppState using the common test helpers
    let db = common::in_memory_db().await;
    use modelrouter::{
        api::{app::{AppState, build_router}, auth::hash_token},
        db::models::NewUser,
        providers::registry::ProviderRegistry,
        router::{cost::CostCalculator, engine::RequestRouter, policy::PolicyEngine},
    };
    use modelrouter::db::repositories::users::UserRepository;
    use std::collections::HashMap;

    let api_key = "test-span-key";
    let hash = hash_token(api_key);
    db.create(NewUser {
        name: "span-test-user".to_string(),
        api_key_hash: hash,
        group_name: None,
    }).await.unwrap();

    let mut mock_providers = HashMap::new();
    mock_providers.insert("mock".to_string(), modelrouter::config::schema::ProviderConfig {
        api_key: "mock".to_string(),
        api_base: Some("http://mock".to_string()),
        timeout_secs: 10,
        api_version: None,
        region: None,
    });
    let settings = Arc::new(modelrouter::config::schema::Settings {
        routing: modelrouter::config::schema::RoutingConfig {
            default_provider: "mock".to_string(),
            ..Default::default()
        },
        providers: mock_providers,
        ..Default::default()
    });

    let db: Arc<dyn modelrouter::api::app::DatabaseProvider> = Arc::new(db);

    // Use MockAdapter via registry — new_with_mock takes a single mock adapter arg
    let registry = Arc::new(ProviderRegistry::new_with_mock(
        common::MockAdapter { response: "hello".to_string() },
    ));

    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default()
    ));

    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] }
        )
    );

    let state = AppState {
        settings: settings.clone(),
        db: db.clone(),
        pool: None,
        router: Arc::new(RequestRouter::new(settings.clone())),
        cost_calc: Arc::new(CostCalculator::new()),
        provider_registry: registry,
        policy: Arc::new(PolicyEngine::new(db.clone())),
        fallback: Arc::new(modelrouter::router::fallback::FallbackChain::new(std::collections::HashMap::new())),
        complexity_router: Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None)),
        response_cache,
        embedding_registry,
        load_balancer: Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
            std::collections::HashMap::new(),
        )),
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        app_metrics: None,
    };

    let server = TestServer::new(build_router(state)).unwrap();
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {api_key}").parse().unwrap(),
        )
        .json(&serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);

    // Give the span time to be processed
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _ = provider.force_flush();

    let spans = exporter.get_finished_spans().unwrap();
    let completions_span = spans.iter().find(|s| s.name == "chat_completions");
    assert!(completions_span.is_some(), "chat_completions span not found. Spans: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>());

    let span = completions_span.unwrap();
    let attr_keys: Vec<&str> = span.attributes.iter()
        .map(|kv| kv.key.as_str())
        .collect();

    assert!(attr_keys.contains(&"model"), "missing 'model' attribute");
    assert!(attr_keys.contains(&"provider"), "missing 'provider' attribute");
    assert!(attr_keys.contains(&"cost.usd"), "missing 'cost.usd' attribute");
    assert!(attr_keys.contains(&"tokens.prompt"), "missing 'tokens.prompt' attribute");
}
