//! Lightweight Prometheus metrics for modelrouter.
//! Enabled via `--features prometheus`.
pub mod prometheus;

#[cfg(feature = "prometheus")]
use ::prometheus::{CounterVec, Opts, Registry};

#[cfg(feature = "prometheus")]
pub struct AppMetrics {
    pub registry: Registry,
    pub requests_total: CounterVec,
    pub tokens_total: CounterVec,
    pub cost_usd_total: CounterVec,
}

#[cfg(feature = "prometheus")]
impl AppMetrics {
    pub fn new() -> anyhow::Result<Self> {
        let registry = ::prometheus::Registry::new();

        let requests_total = CounterVec::new(
            Opts::new("requests_total", "Total proxy requests").namespace("modelrouter"),
            &["model", "provider", "status"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        let tokens_total = CounterVec::new(
            Opts::new("tokens_total", "Total tokens processed").namespace("modelrouter"),
            &["model", "provider", "direction"],
        )?;
        registry.register(Box::new(tokens_total.clone()))?;

        let cost_usd_total = CounterVec::new(
            Opts::new("cost_usd_total", "Total cost in USD").namespace("modelrouter"),
            &["model", "provider"],
        )?;
        registry.register(Box::new(cost_usd_total.clone()))?;

        Ok(Self { registry, requests_total, tokens_total, cost_usd_total })
    }

    pub fn record_request(&self, model: &str, provider: &str, status: &str) {
        self.requests_total.with_label_values(&[model, provider, status]).inc();
    }

    pub fn record_tokens(&self, model: &str, provider: &str, prompt: u32, completion: u32) {
        self.tokens_total.with_label_values(&[model, provider, "prompt"]).inc_by(prompt as f64);
        self.tokens_total.with_label_values(&[model, provider, "completion"]).inc_by(completion as f64);
    }

    pub fn record_cost(&self, model: &str, provider: &str, cost: f64) {
        self.cost_usd_total.with_label_values(&[model, provider]).inc_by(cost);
    }
}
