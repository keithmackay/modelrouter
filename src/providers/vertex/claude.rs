//! Pure JSON translation between OpenAI chat format and Vertex's Anthropic
//! Messages dialect (Claude-on-Vertex). No HTTP, no async, no state.
//!
//! Vertex's Anthropic endpoint differs from direct Anthropic in two ways:
//!   1. `model` goes in the URL, not the body.
//!   2. Body must include `"anthropic_version": "vertex-2023-10-16"`.
//!
//! Content arrays go through the shared Anthropic translation, including
//! OpenAI `image_url` parts (issue #86). Vertex accepts only base64 image
//! sources, so a request carrying an image URL is refused before it is sent.

use crate::providers::adapter::{CompletionResult, NormalizedRequest};
use crate::providers::anthropic::{
    apply_reasoning, find_url_image_source, map_stop_reason, text_from_content,
    thinking_tokens_from_usage, tool_calls_from_content, translate_messages, translate_tool_choice,
    translate_tools,
};

/// Vertex-specific anthropic_version required on every Claude-on-Vertex call.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Anthropic requires `max_tokens`; when the caller doesn't supply one, fall
/// back to the direct Anthropic adapter's default (one constant, so the value
/// sent and the value reported in `x_router.settings` cannot drift).
pub(crate) use crate::providers::anthropic::DEFAULT_MAX_TOKENS;

/// Translate an OpenAI-shaped request to a Vertex Anthropic `:rawPredict` /
/// `:streamRawPredict` body.
/// NOTE: `model` is intentionally omitted — it lives in the URL path.
///
/// `streaming` must be true for a `:streamRawPredict` call: unlike Gemini
/// (where streaming is selected by the URL alone), the Anthropic backend
/// behind `:streamRawPredict` only emits SSE when the body carries
/// `"stream": true`. Without it the endpoint returns one complete non-SSE
/// JSON message, every line of which `translate_sse_line` drops — the client
/// sees a 200 with an empty body.
///
/// Errors when an image arrives as a URL: Claude on Vertex accepts only
/// base64 image sources, and forwarding a `url` source earns an opaque
/// upstream 400. The message is phrased `returned 400` so the retry
/// classifier reads it as a client error (no retry, no circuit-breaker
/// count) and a fallback to a backend that does accept URLs stays possible.
pub fn translate_request(
    req: &NormalizedRequest,
    streaming: bool,
) -> anyhow::Result<serde_json::Value> {
    let (system_text, messages) = translate_messages(&req.messages);
    // The URL itself is deliberately not echoed: the retry classifier
    // matches status digits as substrings, so a URL containing e.g. `500`
    // would misread this client error as a server fault.
    if find_url_image_source(&messages).is_some() {
        anyhow::bail!(
            "modelrouter returned 400 Bad Request before contacting Vertex AI: \
             Claude on Vertex accepts only base64 image sources, but the request \
             carries an image as a URL; send it inline as a data URL \
             (data:<media_type>;base64,<data>)"
        );
    }

    let mut body = serde_json::json!({
        "anthropic_version": VERTEX_ANTHROPIC_VERSION,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
    });
    if streaming {
        body["stream"] = serde_json::json!(true);
    }
    if let Some(system) = system_text {
        body["system"] = serde_json::json!(system);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = serde_json::json!(t);
    }
    // Same Anthropic dialect as the direct adapter: OpenAI tools translate to
    // native tools; tool_choice only rides along with them (issue #88).
    if let Some(tools) = &req.tools {
        body["tools"] = serde_json::Value::Array(translate_tools(tools));
        if let Some(tc) = req.tool_choice.as_ref().and_then(translate_tool_choice) {
            body["tool_choice"] = tc;
        }
    }
    // Reasoning controls (`thinking` / `output_config.effort`) in the same
    // dialect as the direct adapter; omitted when none was resolved.
    apply_reasoning(&mut body, req);
    Ok(body)
}

/// Parse a Vertex Anthropic non-streaming response into the shared `CompletionResult`.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let usage = &v["usage"];
    Ok(CompletionResult {
        content: text_from_content(&v["content"]),
        prompt_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
        cache_read_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
        cache_write_tokens: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32,
        reasoning_tokens: thinking_tokens_from_usage(usage),
        // The adapter, which timed the HTTP send, fills this in.
        ttft_ms: None,
        finish_reason: map_stop_reason(v["stop_reason"].as_str().unwrap_or("end_turn")),
        tool_calls: tool_calls_from_content(&v["content"]),
    })
}

// Streaming translation for Claude-on-Vertex lives in
// `crate::providers::anthropic::AnthropicSseTranslator`: Vertex's Anthropic
// dialect uses the identical SSE event vocabulary (`message_start`,
// `content_block_delta`, `message_delta`), and the shared stateful translator
// folds the split usage report (`input_tokens` on message_start,
// `output_tokens` on message_delta) into a `usage` object on the final chunk
// so the streaming ledger records provider-counted tokens, not estimates
// (issue #84).

#[cfg(test)]
mod reasoning_tests {
    use super::*;
    use crate::providers::adapter::ReasoningControl;

    fn req(reasoning: Option<ReasoningControl>) -> NormalizedRequest {
        NormalizedRequest {
            model: "claude-sonnet-5".into(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            reasoning,
            ..Default::default()
        }
    }

    #[test]
    fn disable_thinking_reaches_the_vertex_body() {
        let body = translate_request(
            &req(Some(ReasoningControl { disable_thinking: true, effort: None })),
            false,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn effort_reaches_the_vertex_body() {
        let body = translate_request(
            &req(Some(ReasoningControl { disable_thinking: false, effort: Some("low") })),
            true,
        )
        .unwrap();
        assert_eq!(body["output_config"]["effort"], "low");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn no_reasoning_control_leaves_the_body_untouched() {
        let body = translate_request(&req(None), false).unwrap();
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn thinking_tokens_are_reported_as_reasoning_tokens() {
        let v = serde_json::json!({
            "content": [{"type": "text", "text": "4"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 197,
                      "output_tokens_details": {"thinking_tokens": 190}}
        });
        let r = parse_response(v).unwrap();
        assert_eq!(r.completion_tokens, 197);
        assert_eq!(r.reasoning_tokens, Some(190));
    }

    #[test]
    fn absent_thinking_breakdown_reports_no_reasoning_tokens() {
        let v = serde_json::json!({
            "content": [{"type": "text", "text": "4"}],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        assert_eq!(parse_response(v).unwrap().reasoning_tokens, None);
    }
}

#[cfg(test)]
mod tools_tests {
    use super::*;

    #[test]
    fn tools_translate_into_the_vertex_body(/* issue #88 */) {
        let req = NormalizedRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![serde_json::json!({"role": "user", "content": "weather?"})],
            tools: Some(vec![serde_json::json!({
                "type": "function",
                "function": {"name": "get_weather", "parameters": {"type": "object"}}
            })]),
            tool_choice: Some(serde_json::json!("required")),
            ..Default::default()
        };
        let body = translate_request(&req, false).unwrap();
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["tool_choice"]["type"], "any");
        assert_eq!(body["anthropic_version"], VERTEX_ANTHROPIC_VERSION);
    }

    #[test]
    fn tool_use_response_parses_to_openai_tool_calls(/* issue #88 */) {
        let v = serde_json::json!({
            "content": [
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Oslo"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let r = parse_response(v).unwrap();
        assert_eq!(r.content, "Checking.");
        assert_eq!(r.finish_reason, "tool_calls");
        let calls = r.tool_calls.unwrap();
        assert_eq!(calls[0]["function"]["name"], "get_weather");
    }

    fn image_req(url: &str) -> NormalizedRequest {
        NormalizedRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Describe this diagram."},
                    {"type": "image_url", "image_url": {"url": url}}
                ]
            })],
            ..Default::default()
        }
    }

    #[test]
    fn data_url_image_parts_reach_vertex_as_base64_image_blocks() {
        // Vertex Anthropic 400s on OpenAI `image_url` parts ("Input tag
        // 'image_url' ... invalid"); they must arrive as native image blocks,
        // on both the rawPredict and streamRawPredict bodies (issue #86).
        for streaming in [false, true] {
            let body = translate_request(&image_req("data:image/jpeg;base64,/9j/4AAQ"), streaming)
                .unwrap();
            let blocks = body["messages"][0]["content"].as_array().unwrap();
            assert_eq!(blocks[0]["type"], "text");
            assert_eq!(blocks[1]["type"], "image");
            assert_eq!(blocks[1]["source"]["type"], "base64");
            assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
            assert_eq!(blocks[1]["source"]["data"], "/9j/4AAQ");
        }
    }

    #[test]
    fn https_image_urls_are_refused_loudly_as_a_client_error() {
        // Vertex accepts only base64 image sources; a `url` source must not
        // be forwarded to fail upstream with an opaque 400 (issue #86).
        for streaming in [false, true] {
            let err = translate_request(&image_req("https://example.com/500/x.png"), streaming)
                .unwrap_err()
                .to_string();
            assert!(err.contains("only base64 image sources"), "{err}");
            assert!(err.contains("data:<media_type>;base64"), "{err}");
            let kind = crate::router::retry::RetryableError::classify(&err);
            assert!(
                matches!(kind, crate::router::retry::RetryableError::ClientError(400)),
                "must classify as a client error, got {kind:?}"
            );
        }
    }

    #[test]
    fn plain_response_keeps_no_tool_calls(/* issue #88 */) {
        let v = serde_json::json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let r = parse_response(v).unwrap();
        assert!(r.tool_calls.is_none());
        assert_eq!(r.finish_reason, "end_turn");
    }
}
