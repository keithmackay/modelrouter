use opentelemetry::{
    trace::{Link, SamplingDecision, SamplingResult, SpanKind, TraceId, TraceState},
    Context, KeyValue, Value,
};
use opentelemetry_sdk::trace::ShouldSample;

/// Head-based sampler. Decisions at span start:
/// 1. Propagate parent sampling flag if parent context is present.
/// 2. Always sample if `modelrouter.force_sample = true` is in creation attributes.
/// 3. Deterministic ratio sampling via trace ID.
#[derive(Debug, Clone)]
pub struct SmartSampler {
    sample_ratio: f64,
}

impl SmartSampler {
    pub fn new(sample_ratio: f64) -> Self {
        Self {
            sample_ratio: sample_ratio.clamp(0.0, 1.0),
        }
    }

    /// Deterministic: same trace ID always maps to same decision.
    fn ratio_sample(&self, trace_id: TraceId) -> bool {
        if self.sample_ratio >= 1.0 {
            return true;
        }
        if self.sample_ratio <= 0.0 {
            return false;
        }
        let id = u64::from_be_bytes(
            trace_id.to_bytes()[8..]
                .try_into()
                .unwrap_or([0u8; 8]),
        );
        let threshold = (self.sample_ratio * u64::MAX as f64) as u64;
        id < threshold
    }
}

impl ShouldSample for SmartSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        trace_id: TraceId,
        _name: &str,
        _span_kind: &SpanKind,
        attributes: &[KeyValue],
        _links: &[Link],
    ) -> SamplingResult {
        use opentelemetry::trace::TraceContextExt;

        // 1. Honour parent sampling decision if parent span is valid
        if let Some(ctx) = parent_context {
            let span = ctx.span();
            let sc = span.span_context();
            if sc.is_valid() {
                let decision = if sc.is_sampled() {
                    SamplingDecision::RecordAndSample
                } else {
                    SamplingDecision::Drop
                };
                return SamplingResult {
                    decision,
                    attributes: Vec::new(),
                    trace_state: sc.trace_state().clone(),
                };
            }
        }

        // 2. Force-sample attribute set at span creation
        let forced = attributes.iter().any(|kv| {
            kv.key.as_str() == "modelrouter.force_sample"
                && kv.value == Value::Bool(true)
        });
        if forced {
            return SamplingResult {
                decision: SamplingDecision::RecordAndSample,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            };
        }

        // 3. Ratio sampling
        SamplingResult {
            decision: if self.ratio_sample(trace_id) {
                SamplingDecision::RecordAndSample
            } else {
                SamplingDecision::Drop
            },
            attributes: Vec::new(),
            trace_state: TraceState::default(),
        }
    }

}
