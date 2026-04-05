pub mod openai_moderation;

use serde_json::Value;

/// Context passed to every guardrail check.
pub struct GuardrailContext {
    /// The messages array from the request body.
    pub messages: Value,
    pub model: String,
    pub user_id: i64,
}

/// Decision returned by a guardrail.
pub enum GuardrailDecision {
    Allow,
    Block { reason: String },
    Replace { content: String },
}

/// Trait every guardrail must implement.
#[async_trait::async_trait]
pub trait Guardrail: Send + Sync {
    fn name(&self) -> &str;
    async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision;
    async fn check_response(&self, ctx: &GuardrailContext, response: &str) -> GuardrailDecision;
}

/// Ordered chain of guardrails. Each entry is `(guardrail, fail_open)`.
/// `fail_open = true` means: if the guardrail errors internally, Allow.
/// The chain short-circuits on the first Block or Replace decision.
pub struct GuardrailChain {
    guardrails: Vec<(Box<dyn Guardrail>, bool)>,
}

impl GuardrailChain {
    pub fn new(guardrails: Vec<(Box<dyn Guardrail>, bool)>) -> Self {
        Self { guardrails }
    }

    /// Run all guardrails against the request. Returns the first non-Allow decision,
    /// or Allow if all pass.
    pub async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision {
        for (guardrail, fail_open) in &self.guardrails {
            let decision = guardrail.check_request(ctx).await;
            match decision {
                GuardrailDecision::Allow => continue,
                other => {
                    let _ = fail_open; // used by guardrail implementations, not the chain runner
                    return other;
                }
            }
        }
        GuardrailDecision::Allow
    }

    /// Run all guardrails against the response. Returns the first non-Allow decision,
    /// or Allow if all pass.
    pub async fn check_response(&self, ctx: &GuardrailContext, response: &str) -> GuardrailDecision {
        for (guardrail, fail_open) in &self.guardrails {
            let decision = guardrail.check_response(ctx, response).await;
            match decision {
                GuardrailDecision::Allow => continue,
                other => {
                    let _ = fail_open;
                    return other;
                }
            }
        }
        GuardrailDecision::Allow
    }
}
