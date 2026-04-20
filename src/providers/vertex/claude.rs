//! Pure JSON translation between OpenAI chat format and Vertex's Anthropic
//! Messages dialect (Claude-on-Vertex). No HTTP, no async, no state.
//!
//! Vertex's Anthropic endpoint differs from direct Anthropic in two ways:
//!   1. `model` goes in the URL, not the body.
//!   2. Body must include `"anthropic_version": "vertex-2023-10-16"`.
//!
//! MVP scope: string-only message content (consistent with `gemini.rs`).

use bytes::Bytes;
use crate::providers::adapter::{CompletionResult, NormalizedRequest};
use crate::providers::anthropic::translate_messages;

/// Vertex-specific anthropic_version required on every Claude-on-Vertex call.
pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";

/// Anthropic requires `max_tokens`; when the caller doesn't supply one, fall
/// back to this value. Matches the direct Anthropic adapter's default.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Translate an OpenAI-shaped request to a Vertex Anthropic `:rawPredict` body.
/// NOTE: `model` is intentionally omitted — it lives in the URL path.
pub fn translate_request(req: &NormalizedRequest) -> serde_json::Value {
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
        finish_reason: v["stop_reason"]
            .as_str()
            .unwrap_or("end_turn")
            .to_string(),
    })
}

/// Translate a single Anthropic SSE event line to an OpenAI `chat.completion.chunk` line.
///
/// Emits:
///   - `data: {...}\n\n` on `content_block_delta` with `text_delta` content.
///   - A trailing `data: {...}\n\ndata: [DONE]\n\n` on `message_stop`.
/// Returns `None` for all other event types (e.g. `message_start`, `ping`).
pub fn translate_sse_line(line: &str) -> Option<Bytes> {
    let payload = line.strip_prefix("data: ")?;
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match v["type"].as_str()? {
        "content_block_delta" if v["delta"]["type"] == "text_delta" => {
            let text = v["delta"]["text"].as_str()?;
            let chunk = serde_json::json!({
                "id": "chatcmpl-vertex-stream",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
            });
            Some(Bytes::from(format!("data: {}\n\n", chunk)))
        }
        "message_stop" => {
            let chunk = serde_json::json!({
                "id": "chatcmpl-vertex-stream",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            Some(Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", chunk)))
        }
        _ => None,
    }
}
