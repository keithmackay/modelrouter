use std::sync::OnceLock;
use opentelemetry::{
    metrics::{Counter, Histogram, Meter},
    KeyValue,
};

pub struct Instruments {
    pub requests_total:      Counter<u64>,
    pub tokens_prompt:       Counter<u64>,
    pub tokens_completion:   Counter<u64>,
    pub cost_usd:            Counter<f64>,
    pub request_duration_ms: Histogram<f64>,
    pub policy_denied:       Counter<u64>,
    pub hooks_duration_ms:   Histogram<f64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Call once at startup (inside init_telemetry) to create all instruments.
pub fn init_instruments(meter: Meter) {
    let instruments = Instruments {
        requests_total: meter
            .u64_counter("modelrouter.requests.total")
            .with_description("Total number of proxy requests")
            .build(),
        tokens_prompt: meter
            .u64_counter("modelrouter.tokens.prompt")
            .with_description("Total prompt tokens processed")
            .build(),
        tokens_completion: meter
            .u64_counter("modelrouter.tokens.completion")
            .with_description("Total completion tokens generated")
            .build(),
        cost_usd: meter
            .f64_counter("modelrouter.cost.usd")
            .with_description("Total cost in USD")
            .build(),
        request_duration_ms: meter
            .f64_histogram("modelrouter.request.duration_ms")
            .with_description("Request latency in milliseconds")
            .build(),
        policy_denied: meter
            .u64_counter("modelrouter.policy.denied")
            .with_description("Requests denied by policy engine")
            .build(),
        hooks_duration_ms: meter
            .f64_histogram("modelrouter.hooks.duration_ms")
            .with_description("Hook execution latency in milliseconds")
            .build(),
    };
    // Ignore error if called twice (e.g. in tests)
    let _ = INSTRUMENTS.set(instruments);
}

fn get() -> Option<&'static Instruments> {
    INSTRUMENTS.get()
}

// ── Recording helpers ──────────────────────────────────────────────────

/// status: "ok" | "error" | "policy_denied"
pub fn record_request(model: &str, provider: &str, status: &str) {
    if let Some(i) = get() {
        i.requests_total.add(1, &[
            KeyValue::new("model", model.to_string()),
            KeyValue::new("provider", provider.to_string()),
            KeyValue::new("status", status.to_string()),
        ]);
    }
}

pub fn record_tokens(model: &str, provider: &str, prompt: u32, completion: u32) {
    if let Some(i) = get() {
        let attrs = &[
            KeyValue::new("model", model.to_string()),
            KeyValue::new("provider", provider.to_string()),
        ];
        i.tokens_prompt.add(prompt as u64, attrs);
        i.tokens_completion.add(completion as u64, attrs);
    }
}

pub fn record_cost(model: &str, provider: &str, user_id: i64, cost: f64) {
    if let Some(i) = get() {
        i.cost_usd.add(cost, &[
            KeyValue::new("model", model.to_string()),
            KeyValue::new("provider", provider.to_string()),
            KeyValue::new("user_id", user_id.to_string()),
        ]);
    }
}

pub fn record_duration(model: &str, provider: &str, streaming: bool, ms: f64) {
    if let Some(i) = get() {
        i.request_duration_ms.record(ms, &[
            KeyValue::new("model", model.to_string()),
            KeyValue::new("provider", provider.to_string()),
            KeyValue::new("streaming", streaming.to_string()),
        ]);
    }
}

/// reason: "budget" | "rate_limit" | "model_denied"
pub fn record_policy_denied(reason: &str) {
    if let Some(i) = get() {
        i.policy_denied.add(1, &[
            KeyValue::new("reason", reason.to_string()),
        ]);
    }
}

/// hook_type: "pipeline" | "lifecycle"
pub fn record_hook_duration(hook_name: &str, hook_type: &str, ms: f64) {
    if let Some(i) = get() {
        i.hooks_duration_ms.record(ms, &[
            KeyValue::new("hook_name", hook_name.to_string()),
            KeyValue::new("hook_type", hook_type.to_string()),
        ]);
    }
}

/// Returns the 7 instrument names this module registers.
/// Used in tests to verify instruments are properly named.
#[cfg(test)]
pub const INSTRUMENT_NAMES: &[&str] = &[
    "modelrouter.requests.total",
    "modelrouter.tokens.prompt",
    "modelrouter.tokens.completion",
    "modelrouter.cost.usd",
    "modelrouter.request.duration_ms",
    "modelrouter.policy.denied",
    "modelrouter.hooks.duration_ms",
];
