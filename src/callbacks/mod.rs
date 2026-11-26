pub mod langfuse;
pub mod langsmith;
pub mod outbox;
pub mod usage;
pub mod webhook;

use serde_json::Value;

#[derive(Clone)]
pub struct CallbackEvent {
    pub trace_id: String,
    pub user_id: i64,
    pub model: String,
    pub provider: String,
    pub input: Value,
    pub output: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cost_usd: f64,
    pub latency_ms: i64,
}

pub trait CallbackBackend: Send + Sync {
    fn send(&self, event: CallbackEvent);

    /// A `usage` event (see [`usage`]). Backends that do not handle usage
    /// events ignore it.
    fn send_usage(&self, _payload: &Value) {}
}

pub struct CallbackDispatcher {
    backends: Vec<Box<dyn CallbackBackend>>,
}

impl CallbackDispatcher {
    pub fn new(backends: Vec<Box<dyn CallbackBackend>>) -> Self {
        Self { backends }
    }

    pub fn dispatch(&self, event: CallbackEvent) {
        for backend in &self.backends {
            backend.send(event.clone());
        }
    }

    /// Emit the `usage` event for one stored ledger row to every backend.
    pub fn dispatch_usage(&self, row: &crate::db::models::CostLedgerEntry) {
        if self.backends.is_empty() {
            return;
        }
        let payload = usage::from_ledger(row);
        for backend in &self.backends {
            backend.send_usage(&payload);
        }
    }
}
