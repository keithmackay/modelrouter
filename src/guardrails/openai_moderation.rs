use super::{Guardrail, GuardrailContext, GuardrailDecision};

pub struct OpenAIModerationGuardrail {
    api_key: String,
    /// If true, HTTP/parse errors cause Allow. If false, they cause Block.
    fail_open: bool,
}

impl OpenAIModerationGuardrail {
    pub fn new(api_key: String) -> Self {
        Self { api_key, fail_open: true }
    }

    pub fn with_fail_open(api_key: String, fail_open: bool) -> Self {
        Self { api_key, fail_open }
    }

    /// Extract plain text from a messages array for moderation.
    fn messages_to_text(messages: &serde_json::Value) -> String {
        messages
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    async fn moderate(&self, text: &str) -> GuardrailDecision {
        if self.api_key.is_empty() || text.is_empty() {
            return GuardrailDecision::Allow;
        }
        let client = reqwest::Client::new();
        let body = serde_json::json!({ "input": text });
        let resp = match client
            .post("https://api.openai.com/v1/moderations")
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, fail_open = self.fail_open, "OpenAI moderation request failed");
                return if self.fail_open {
                    GuardrailDecision::Allow
                } else {
                    GuardrailDecision::Block { reason: format!("moderation check failed: {}", e) }
                };
            }
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, fail_open = self.fail_open, "Failed to parse moderation response");
                return if self.fail_open {
                    GuardrailDecision::Allow
                } else {
                    GuardrailDecision::Block { reason: format!("moderation parse failed: {}", e) }
                };
            }
        };
        let flagged = json["results"][0]["flagged"].as_bool().unwrap_or(false);
        if flagged {
            GuardrailDecision::Block {
                reason: "content flagged by OpenAI moderation".to_string(),
            }
        } else {
            GuardrailDecision::Allow
        }
    }
}

#[async_trait::async_trait]
impl Guardrail for OpenAIModerationGuardrail {
    fn name(&self) -> &str { "openai-moderation" }

    async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision {
        let text = Self::messages_to_text(&ctx.messages);
        self.moderate(&text).await
    }

    async fn check_response(&self, _ctx: &GuardrailContext, response: &str) -> GuardrailDecision {
        self.moderate(response).await
    }
}
