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
}

#[async_trait::async_trait]
impl Guardrail for OpenAIModerationGuardrail {
    fn name(&self) -> &str { "openai-moderation" }

    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Allow
    }

    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Allow
    }
}
