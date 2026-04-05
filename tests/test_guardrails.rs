mod common;

use modelrouter::guardrails::{
    GuardrailChain, GuardrailContext, GuardrailDecision,
};

#[tokio::test]
async fn empty_chain_allows_everything() {
    let chain = GuardrailChain::new(vec![]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([{"role": "user", "content": "Hello"}]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_request(&ctx).await;
    assert!(matches!(decision, GuardrailDecision::Allow));
}

#[tokio::test]
async fn empty_chain_allows_response() {
    let chain = GuardrailChain::new(vec![]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([{"role": "user", "content": "Hello"}]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_response(&ctx, "Hello back").await;
    assert!(matches!(decision, GuardrailDecision::Allow));
}

struct AlwaysBlockGuardrail;

#[async_trait::async_trait]
impl modelrouter::guardrails::Guardrail for AlwaysBlockGuardrail {
    fn name(&self) -> &str { "always-block" }
    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Block { reason: "blocked".to_string() }
    }
    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Block { reason: "blocked".to_string() }
    }
}

#[tokio::test]
async fn chain_with_blocking_guardrail_returns_block() {
    let chain = GuardrailChain::new(vec![
        (Box::new(AlwaysBlockGuardrail) as Box<dyn modelrouter::guardrails::Guardrail>, false),
    ]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_request(&ctx).await;
    assert!(matches!(decision, GuardrailDecision::Block { .. }));
}

struct AlwaysReplaceGuardrail;

#[async_trait::async_trait]
impl modelrouter::guardrails::Guardrail for AlwaysReplaceGuardrail {
    fn name(&self) -> &str { "always-replace" }
    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Allow
    }
    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Replace { content: "[redacted]".to_string() }
    }
}

#[tokio::test]
async fn chain_replace_decision_is_returned_for_response() {
    let chain = GuardrailChain::new(vec![
        (Box::new(AlwaysReplaceGuardrail) as Box<dyn modelrouter::guardrails::Guardrail>, false),
    ]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_response(&ctx, "some response").await;
    assert!(matches!(decision, GuardrailDecision::Replace { content } if content == "[redacted]"));
}
