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
