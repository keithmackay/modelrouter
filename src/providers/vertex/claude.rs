//! Pure JSON translation between OpenAI chat format and Vertex's Anthropic
//! Messages dialect (Claude-on-Vertex). No HTTP, no async, no state.
//!
//! Vertex's Anthropic endpoint differs from direct Anthropic in two ways:
//!   1. `model` goes in the URL, not the body.
//!   2. Body must include `"anthropic_version": "vertex-2023-10-16"`.
//!
//! MVP scope: string-only message content (consistent with `gemini.rs`).

use crate::providers::adapter::{CompletionResult, NormalizedRequest};
use crate::providers::anthropic::{
    map_stop_reason, text_from_content, tool_calls_from_content, translate_messages,
    translate_tool_choice, translate_tools,
};

/// Vertex-specific anthropic_version required on every Claude-on-Vertex call.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Anthropic requires `max_tokens`; when the caller doesn't supply one, fall
/// back to this value. Matches the direct Anthropic adapter's default.
const DEFAULT_MAX_TOKENS: u32 = 4096;

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
pub fn translate_request(req: &NormalizedRequest, streaming: bool) -> serde_json::Value {
    let (system_text, messages) = translate_messages(&req.messages);

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
    body
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
        let body = translate_request(&req, false);
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

    #[test]
    fn image_url_parts_reach_vertex_as_anthropic_image_blocks() {
        // Vertex Anthropic 400s on OpenAI `image_url` parts ("Input tag
        // 'image_url' ... invalid"); they must arrive as native image blocks.
        let req = NormalizedRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Describe this diagram."},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}},
                    {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
                ]
            })],
            ..Default::default()
        };
        let body = translate_request(&req, false);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(blocks[1]["source"]["data"], "/9j/4AAQ");
        assert_eq!(blocks[2]["type"], "image");
        assert_eq!(blocks[2]["source"]["type"], "url");
        assert_eq!(blocks[2]["source"]["url"], "https://example.com/x.png");
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
