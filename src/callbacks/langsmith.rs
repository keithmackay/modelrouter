use super::{CallbackBackend, CallbackEvent};
use crate::config::schema::LangSmithConfig;

pub struct LangSmithBackend {
    config: LangSmithConfig,
    client: reqwest::Client,
}

impl LangSmithBackend {
    pub fn new(config: LangSmithConfig) -> Self {
        Self { config, client: reqwest::Client::new() }
    }
}

impl CallbackBackend for LangSmithBackend {
    fn send(&self, event: CallbackEvent) {
        let url = format!("{}/runs", self.config.host);
        let api_key = self.config.api_key.clone();
        let project = self.config.project.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({
                "id": event.trace_id,
                "name": "modelrouter.completion",
                "run_type": "llm",
                "inputs": { "messages": event.input },
                "outputs": { "content": event.output },
                "extra": {
                    "model": event.model, "provider": event.provider,
                    "prompt_tokens": event.prompt_tokens,
                    "completion_tokens": event.completion_tokens,
                    "cost_usd": event.cost_usd, "latency_ms": event.latency_ms,
                    "session_name": project,
                }
            });
            if let Err(e) = client.post(&url).header("x-api-key", &api_key)
                .json(&body).send().await
            {
                tracing::warn!("langsmith callback failed: {e}");
            }
        });
    }
}
