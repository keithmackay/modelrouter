use serde_json::Value;
use crate::config::schema::ComplexityRoutingConfig;

pub struct ComplexityRouter {
    config: Option<ComplexityRoutingConfig>,
}

impl ComplexityRouter {
    pub fn new(config: Option<ComplexityRoutingConfig>) -> Self {
        Self { config }
    }

    /// Returns the model to use — either the requested model or the cheap model
    /// if token count exceeds the configured threshold.
    pub fn maybe_downgrade(&self, requested_model: &str, messages: &[Value]) -> String {
        let config = match &self.config {
            Some(c) if c.enabled => c,
            _ => return requested_model.to_string(),
        };

        let estimated = estimate_tokens_from_messages(messages);
        if estimated > config.token_threshold as usize {
            config.cheap_model.clone()
        } else {
            requested_model.to_string()
        }
    }
}

/// Estimate token count from messages using chars/4 heuristic.
pub fn estimate_tokens_from_messages(messages: &[Value]) -> usize {
    messages.iter().map(|m| {
        m["content"].as_str().map(|s| s.chars().count() / 4).unwrap_or(0)
    }).sum()
}
