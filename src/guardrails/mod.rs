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
///
/// The `fail_open` bool is passed through to each guardrail implementation via
/// `with_fail_open` constructors. The chain runner itself does not enforce it —
/// each guardrail is responsible for honouring the flag when handling internal
/// errors (e.g. network failures). This design means guardrails that do not
/// implement `fail_open` internally will not benefit from the flag.
///
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
        for (guardrail, _fail_open) in &self.guardrails {
            let decision = guardrail.check_request(ctx).await;
            match decision {
                GuardrailDecision::Allow => continue,
                other => return other,
            }
        }
        GuardrailDecision::Allow
    }

    /// Run all guardrails against the response. Returns the first non-Allow decision,
    /// or Allow if all pass.
    pub async fn check_response(&self, ctx: &GuardrailContext, response: &str) -> GuardrailDecision {
        for (guardrail, _fail_open) in &self.guardrails {
            let decision = guardrail.check_response(ctx, response).await;
            match decision {
                GuardrailDecision::Allow => continue,
                other => return other,
            }
        }
        GuardrailDecision::Allow
    }
}
