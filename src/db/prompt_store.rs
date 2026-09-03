//! Storage policy for the prompt log (issue #4).
//!
//! Every route that records a `NewPrompt` passes it through
//! [`apply_storage_policy`] before insert. The policy is a pure function so
//! the redaction/skip behavior is testable without a database, and so a new
//! call site cannot get it subtly wrong by re-implementing the rules inline.
//!
//! Cost tracking is deliberately OUT of scope here: `cost_ledger` rows are
//! written regardless of prompt-log policy — when the log row is skipped the
//! ledger entry simply carries `prompt_id: NULL`.

use crate::callbacks::CallbackEvent;
use crate::config::schema::StorageConfig;
use crate::db::models::NewPrompt;

/// Placeholder stored in `messages` when content storage is disabled, so the
/// admin detail view says why there is nothing to show rather than rendering
/// an empty pane that looks like data loss.
pub const CONTENT_NOT_STORED: &str = "(content storage disabled — see [storage] in config.toml)";

/// Apply the `[storage]` policy to a prompt-log row.
///
/// Returns `None` when prompt logging is disabled entirely (callers skip the
/// INSERT and record their cost-ledger entry with `prompt_id: None`).
/// Otherwise returns the row to insert, with message/response content
/// replaced by a placeholder unless `store_prompt_content` is on.
pub fn apply_storage_policy(storage: &StorageConfig, prompt: NewPrompt) -> Option<NewPrompt> {
    if !storage.store_prompts {
        return None;
    }
    let mut prompt = prompt;
    redact_prompt_content(storage, &mut prompt);
    Some(prompt)
}

/// Content half of the policy alone, for call sites whose skip decision is
/// already made elsewhere (completions routes their `store_prompts` gate
/// through the pre-existing `skip_log` machinery shared with `x-no-log`).
pub fn redact_prompt_content(storage: &StorageConfig, prompt: &mut NewPrompt) {
    if !storage.store_prompt_content {
        prompt.messages = CONTENT_NOT_STORED.to_string();
        prompt.response = None;
    }
}

/// The same content policy for the observability egress (issue #53).
///
/// A `CallbackEvent` carries the prompt and response to Langfuse, LangSmith
/// or a webhook. Those are a stricter retention surface than the local
/// database, not a looser one, so an operator who has turned content storage
/// off must not have every prompt shipped to a third party anyway. Both
/// halves live in this module so the row and the event cannot drift apart.
pub fn redact_callback_content(storage: &StorageConfig, event: &mut CallbackEvent) {
    if !storage.store_prompt_content {
        event.input = serde_json::Value::String(CONTENT_NOT_STORED.to_string());
        event.output = CONTENT_NOT_STORED.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NewPrompt {
        NewPrompt {
            user_id: 1,
            session_id: None,
            request_model: "balanced".into(),
            routed_model: "vertex/anthropic/claude".into(),
            provider: "vertex".into(),
            messages: r#"[{"role":"user","content":"secret"}]"#.into(),
            response: Some("secret answer".into()),
            finish_reason: Some("stop".into()),
            prompt_tokens: 10,
            completion_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.01,
            latency_ms: Some(123),
            tags: "[]".into(),
            project: None,
            attribution_correlation_id: None,
            attribution_tags: "[]".into(),
        }
    }

    fn sample_event() -> CallbackEvent {
        CallbackEvent {
            trace_id: "1".into(),
            user_id: 1,
            model: "vertex/anthropic/claude".into(),
            provider: "vertex".into(),
            input: serde_json::json!([{"role": "user", "content": "secret"}]),
            output: "secret answer".into(),
            prompt_tokens: 10,
            completion_tokens: 20,
            cost_usd: 0.01,
            latency_ms: 123,
        }
    }

    #[test]
    fn default_policy_strips_callback_content_keeps_metadata() {
        let cfg = StorageConfig::default();
        let mut ev = sample_event();
        redact_callback_content(&cfg, &mut ev);
        assert_eq!(ev.input, serde_json::Value::String(CONTENT_NOT_STORED.into()));
        assert_eq!(ev.output, CONTENT_NOT_STORED);
        assert_eq!(ev.prompt_tokens, 10);
        assert_eq!(ev.cost_usd, 0.01);
        assert_eq!(ev.model, "vertex/anthropic/claude");
    }

    #[test]
    fn content_enabled_leaves_callback_untouched() {
        let cfg = StorageConfig { store_prompt_content: true, ..Default::default() };
        let mut ev = sample_event();
        redact_callback_content(&cfg, &mut ev);
        assert_eq!(ev.output, "secret answer");
        assert_eq!(ev.input[0]["content"], "secret");
    }

    #[test]
    fn disabled_log_skips_insert() {
        let cfg = StorageConfig { store_prompts: false, ..Default::default() };
        assert!(apply_storage_policy(&cfg, sample()).is_none());
    }

    #[test]
    fn default_policy_strips_content_keeps_metadata() {
        let cfg = StorageConfig::default();
        let stored = apply_storage_policy(&cfg, sample()).expect("row should be stored");
        assert_eq!(stored.messages, CONTENT_NOT_STORED);
        assert_eq!(stored.response, None);
        // Metadata survives redaction.
        assert_eq!(stored.prompt_tokens, 10);
        assert_eq!(stored.completion_tokens, 20);
        assert_eq!(stored.cost_usd, 0.01);
        assert_eq!(stored.routed_model, "vertex/anthropic/claude");
        assert_eq!(stored.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn opt_in_content_storage_preserves_bodies() {
        let cfg = StorageConfig { store_prompt_content: true, ..Default::default() };
        let stored = apply_storage_policy(&cfg, sample()).unwrap();
        assert!(stored.messages.contains("secret"));
        assert_eq!(stored.response.as_deref(), Some("secret answer"));
    }
}
