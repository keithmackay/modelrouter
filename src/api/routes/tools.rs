//! Tool-calling plumbing shared by the OpenAI-compatible completion routes
//! (`/v1/chat/completions` and `/v1/responses`).
//!
//! Both routes accept `tools`/`tool_choice` in the chat-completions shape,
//! gate them on the RESOLVED adapter's capability, and hand them to the
//! adapter through [`NormalizedRequest`](crate::providers::adapter::NormalizedRequest),
//! where each adapter does its own wire translation (issue #88). Keeping the
//! gate and the extraction here means the two routes cannot drift apart
//! again (issue #87: `/v1/responses` kept a stale copy of a blanket
//! rejection long after chat completions had moved on).

use serde_json::Value;

use crate::api::{app::AppState, error::ApiError};

/// Does the request carry a non-empty `tools` array? (issue #88)
pub(crate) fn request_has_tools(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|t| !t.is_empty())
}

/// A `tool_choice` steering the model toward tools is meaningless — and, on
/// the OpenAI surface, invalid — without a `tools` array to choose from.
/// Null and "none" are treated as absent (both mean: do not use tools).
pub(crate) fn validate_tool_choice_requires_tools(body: &Value) -> Result<(), ApiError> {
    if request_has_tools(body) {
        return Ok(());
    }
    if let Some(tc) = body.get("tool_choice") {
        if !tc.is_null() && tc.as_str() != Some("none") {
            return Err(ApiError::InvalidRequest(
                "`tool_choice` requires a non-empty `tools` array".to_string(),
            ));
        }
    }
    Ok(())
}

/// Tools capability gate (issue #88). A request carrying tools is rejected
/// only when the RESOLVED adapter cannot forward them — the only point where
/// that is knowable, since the caller addresses an alias. Silently dropping
/// tools instead would be worse than a 400: the model would answer an
/// agentic request in prose.
pub(crate) fn ensure_tools_supported(
    state: &AppState,
    body: &Value,
    provider_name: &str,
    canonical_model: &str,
) -> Result<(), ApiError> {
    if !request_has_tools(body) {
        return Ok(());
    }
    let adapter = state
        .provider_registry
        .get(provider_name)
        .map_err(ApiError::ProviderError)?;
    if adapter.supports_tools(canonical_model) {
        Ok(())
    } else {
        Err(ApiError::InvalidRequest(format!(
            "`tools` is not supported for model '{canonical_model}' on provider '{provider_name}'"
        )))
    }
}

/// The `tools`/`tool_choice` pair to put on the provider request. Tools ride
/// along only as a pair: a `tool_choice` without tools was rejected at the
/// door, and forwarding one alone would be invalid at the provider.
pub(crate) fn forwarded_tools(body: &Value) -> (Option<Vec<Value>>, Option<Value>) {
    let tools = body["tools"].as_array().filter(|t| !t.is_empty()).cloned();
    let tool_choice = if tools.is_some() {
        body.get("tool_choice").filter(|tc| !tc.is_null()).cloned()
    } else {
        None
    };
    (tools, tool_choice)
}

/// The assistant `message` object for a completion result. OpenAI reports
/// `content: null` (not "") on a pure tool-call turn, and several client SDKs
/// branch on exactly that (issue #88).
pub(crate) fn assistant_message(result: &crate::providers::adapter::CompletionResult) -> Value {
    let mut message = serde_json::json!({
        "role": "assistant",
        "content": if result.content.is_empty() && result.tool_calls.is_some() {
            Value::Null
        } else {
            Value::String(result.content.clone())
        },
    });
    if let Some(tool_calls) = &result.tool_calls {
        message["tool_calls"] = tool_calls.clone();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::adapter::CompletionResult;
    use serde_json::json;

    #[test]
    fn empty_or_absent_tools_do_not_count(/* issue #88 */) {
        assert!(!request_has_tools(&json!({"messages": []})));
        assert!(!request_has_tools(&json!({"tools": []})));
        assert!(request_has_tools(&json!({"tools": [{"type": "function"}]})));
    }

    #[test]
    fn tool_choice_without_tools_is_rejected(/* issue #88 */) {
        let body = json!({"tool_choice": "required"});
        assert!(validate_tool_choice_requires_tools(&body).is_err());
        // null and "none" both mean "no tools" and stay accepted.
        assert!(validate_tool_choice_requires_tools(&json!({"tool_choice": null})).is_ok());
        assert!(validate_tool_choice_requires_tools(&json!({"tool_choice": "none"})).is_ok());
        assert!(validate_tool_choice_requires_tools(&json!({})).is_ok());
        // With tools present any tool_choice shape is the provider's problem.
        assert!(validate_tool_choice_requires_tools(
            &json!({"tools": [{"type": "function"}], "tool_choice": "required"})
        )
        .is_ok());
    }

    #[test]
    fn forwarded_tools_travel_as_a_pair() {
        let (tools, tc) = forwarded_tools(&json!({
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": "auto",
        }));
        assert_eq!(tools.unwrap().len(), 1);
        assert_eq!(tc, Some(json!("auto")));

        let (tools, tc) = forwarded_tools(&json!({"tools": [], "tool_choice": "auto"}));
        assert!(tools.is_none());
        assert!(tc.is_none(), "a tool_choice never rides without tools");

        let (tools, tc) =
            forwarded_tools(&json!({"tools": [{"type": "function"}], "tool_choice": null}));
        assert!(tools.is_some());
        assert!(
            tc.is_none(),
            "an explicit null tool_choice is not forwarded"
        );
    }

    #[test]
    fn assistant_message_reports_null_content_on_a_pure_tool_call_turn() {
        let result = CompletionResult {
            content: String::new(),
            tool_calls: Some(json!([{"id": "c1", "type": "function",
                "function": {"name": "f", "arguments": "{}"}}])),
            ..Default::default()
        };
        let m = assistant_message(&result);
        assert!(m["content"].is_null());
        assert_eq!(m["tool_calls"][0]["id"], "c1");

        let plain = CompletionResult {
            content: "hi".into(),
            ..Default::default()
        };
        let m = assistant_message(&plain);
        assert_eq!(m["content"], "hi");
        assert!(m.get("tool_calls").is_none());
    }
}
