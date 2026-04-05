use super::{CallbackBackend, CallbackEvent};
use crate::config::schema::LangFuseConfig;

pub struct LangFuseBackend {
    config: LangFuseConfig,
    client: reqwest::Client,
}

impl LangFuseBackend {
    pub fn new(config: LangFuseConfig) -> Self {
        Self { config, client: reqwest::Client::new() }
    }
}

impl CallbackBackend for LangFuseBackend {
    fn send(&self, event: CallbackEvent) {
        let url = format!("{}/api/public/traces", self.config.host);
        let public_key = self.config.public_key.clone();
        let secret_key = self.config.secret_key.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({
                "id": event.trace_id,
                "name": "modelrouter.completion",
                "input": event.input,
                "output": event.output,
                "metadata": {
                    "model": event.model, "provider": event.provider,
                    "prompt_tokens": event.prompt_tokens,
                    "completion_tokens": event.completion_tokens,
                    "cost_usd": event.cost_usd, "latency_ms": event.latency_ms,
                    "user_id": event.user_id,
                }
            });
            if let Err(e) = client.post(&url).basic_auth(&public_key, Some(&secret_key))
                .json(&body).send().await
            {
                tracing::warn!("langfuse callback failed: {e}");
            }
        });
    }
}
