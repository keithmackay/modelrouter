use super::{CallbackBackend, CallbackEvent};

pub struct WebhookBackendConfig {
    pub name: String,
    pub url: String,
    pub events: Vec<String>,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

pub struct WebhookBackend {
    config: WebhookBackendConfig,
    client: reqwest::Client,
}

impl WebhookBackend {
    pub fn new(config: WebhookBackendConfig) -> Self {
        Self { config, client: reqwest::Client::new() }
    }

    fn should_send(&self) -> bool {
        self.config.events.is_empty()
            || self.config.events.iter().any(|e| e == "*" || e == "completion")
    }
}

impl CallbackBackend for WebhookBackend {
    fn send(&self, event: CallbackEvent) {
        if !self.should_send() { return; }

        let url = self.config.url.clone();
        let secret_name = self.config.secret_header_name.clone();
        let secret_value = self.config.secret_header_value.clone();
        let client = self.client.clone();
        let name = self.config.name.clone();

        tokio::spawn(async move {
            let body = serde_json::json!({
                "event": "completion",
                "trace_id": event.trace_id,
                "user_id": event.user_id,
                "model": event.model,
                "provider": event.provider,
                "prompt_tokens": event.prompt_tokens,
                "completion_tokens": event.completion_tokens,
                "cost_usd": event.cost_usd,
                "latency_ms": event.latency_ms,
            });

            let mut req = client.post(&url).json(&body);
            if let (Some(h), Some(v)) = (secret_name, secret_value) {
                req = req.header(h, v);
            }
            if let Err(e) = req.send().await {
                tracing::warn!(webhook = name.as_str(), "webhook POST failed: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend(events: Vec<&str>) -> WebhookBackend {
        WebhookBackend::new(WebhookBackendConfig {
            name: "test".into(),
            url: "http://example.com".into(),
            events: events.into_iter().map(str::to_string).collect(),
            secret_header_name: None,
            secret_header_value: None,
        })
    }

    #[test]
    fn wildcard_events_should_send() {
        assert!(make_backend(vec!["*"]).should_send());
    }

    #[test]
    fn completion_event_should_send() {
        assert!(make_backend(vec!["completion"]).should_send());
    }

    #[test]
    fn empty_events_should_send() {
        assert!(make_backend(vec![]).should_send());
    }

    #[test]
    fn non_completion_event_should_not_send() {
        assert!(!make_backend(vec!["budget_exceeded"]).should_send());
    }
}
