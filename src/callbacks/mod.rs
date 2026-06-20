pub mod langfuse;
pub mod langsmith;
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
}
