//! Pure JSON translation between OpenAI chat format and Vertex's Anthropic
//! Messages dialect (Claude-on-Vertex). No HTTP, no async, no state.
//!
//! Vertex's Anthropic endpoint differs from direct Anthropic in two ways:
//!   1. `model` goes in the URL, not the body.
//!   2. Body must include `"anthropic_version": "vertex-2023-10-16"`.
//!
//! MVP scope: string-only message content (consistent with `gemini.rs`).

use crate::providers::adapter::{CompletionResult, NormalizedRequest};
use crate::providers::anthropic::translate_messages;

/// Vertex-specific anthropic_version required on every Claude-on-Vertex call.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Anthropic requires `max_tokens`; when the caller doesn't supply one, fall
/// back to this value. Matches the direct Anthropic adapter's default.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Translate an OpenAI-shaped request to a Vertex Anthropic `:rawPredict` body.
/// NOTE: `model` is intentionally omitted — it lives in the URL path.
pub fn translate_request(req: &NormalizedRequest, stream: bool) -> serde_json::Value {
    let (system_text, messages) = translate_messages(&req.messages);

    let mut body = serde_json::json!({
        "anthropic_version": VERTEX_ANTHROPIC_VERSION,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
    });
    if let Some(system) = system_text {
        body["system"] = serde_json::json!(system);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = serde_json::json!(t);
    }
    // `:streamRawPredict` only serves SSE when the body asks for it; without
    // this flag Vertex answers with one raw JSON object, the SSE translator
    // finds no `data:` lines, and the client receives an empty 200 (#83).
    if stream {
        body["stream"] = serde_json::json!(true);
    }
    body
}

/// Parse a Vertex Anthropic non-streaming response into the shared `CompletionResult`.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let content: String = v["content"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = &v["usage"];
    Ok(CompletionResult {
        content,
        prompt_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
        cache_read_tokens: usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
        cache_write_tokens: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32,
        // The adapter, which timed the HTTP send, fills this in.
        ttft_ms: None,
        finish_reason: v["stop_reason"]
            .as_str()
            .unwrap_or("end_turn")
            .to_string(),
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
