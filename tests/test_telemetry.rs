#![cfg(feature = "otel")]

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
