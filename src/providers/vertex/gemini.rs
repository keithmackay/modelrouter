//! Pure JSON translation between OpenAI chat format and Vertex Gemini
//! `generateContent`. No HTTP, no async, no state — consumed by `VertexAdapter`.
//!
//! MVP scope: string-only message content. Messages with array-shaped
//! `content` (e.g. `[{"type":"text","text":"..."}]`) silently serialize as
//! an empty string; multimodal support is a follow-up.

use bytes::Bytes;
use crate::providers::adapter::{CompletionResult, NormalizedRequest};

/// Translate an OpenAI-shaped request to a Gemini `generateContent` body.
pub fn translate_request(req: &NormalizedRequest) -> serde_json::Value {
    let mut system_parts: Vec<serde_json::Value> = Vec::new();
    let mut contents: Vec<serde_json::Value> = Vec::new();

    for m in &req.messages {
        let role = m["role"].as_str().unwrap_or("");
        let text = m["content"].as_str().unwrap_or("").to_string();
        match role {
            "system" => system_parts.push(serde_json::json!({"text": text})),
            "user" => contents.push(serde_json::json!({
                "role": "user",
                "parts": [{"text": text}]
            })),
            "assistant" => contents.push(serde_json::json!({
                "role": "model",
                "parts": [{"text": text}]
            })),
            _ => {}
        }
    }

    let mut body = serde_json::json!({ "contents": contents });

    if !system_parts.is_empty() {
        body["systemInstruction"] = serde_json::json!({ "parts": system_parts });
    }

    let mut gen_config = serde_json::Map::new();
    if let Some(t) = req.temperature {
        gen_config.insert("temperature".into(), serde_json::json!(t));
    }
    if let Some(m) = req.max_tokens {
        gen_config.insert("maxOutputTokens".into(), serde_json::json!(m));
    }
    if !gen_config.is_empty() {
        body["generationConfig"] = serde_json::Value::Object(gen_config);
    }

    body
}

/// Map a Gemini `finishReason` to OpenAI's vocabulary. Safety-class reasons
/// (`SAFETY`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII`) all collapse to
/// `content_filter`. `RECITATION` (copyright-block truncation) maps to `stop`
/// since the client sees a clean termination. Unknown reasons default to `stop`.
fn map_finish_reason(r: &str) -> &'static str {
    match r {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content_filter",
        "RECITATION" => "stop",
        _ => "stop",
    }
}

/// Parse a Gemini non-streaming response into the shared `CompletionResult`.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let candidate = v["candidates"]
        .get(0)
        .ok_or_else(|| anyhow::anyhow!("Gemini response has no candidates"))?;
    let content: String = candidate["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish = candidate["finishReason"].as_str().unwrap_or("STOP");
    let usage = &v["usageMetadata"];
    let prompt = usage["promptTokenCount"].as_u64().unwrap_or(0) as u32;
    let completion = usage["candidatesTokenCount"].as_u64().unwrap_or(0) as u32;
    let cache_read = usage["cachedContentTokenCount"].as_u64().unwrap_or(0) as u32;
    Ok(CompletionResult {
        content,
        prompt_tokens: prompt,
        completion_tokens: completion,
        finish_reason: map_finish_reason(finish).to_string(),
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        reasoning_tokens: usage["thoughtsTokenCount"].as_u64().map(|n| n as u32),
        // The adapter, which timed the HTTP send, fills this in.
        ttft_ms: None,
        tool_calls: None,
    })
}

/// Translate a single Gemini SSE line to an OpenAI `chat.completion.chunk` line.
/// Returns `None` for comments, blank lines, non-data events, and the final
/// usage-only chunk (which has `usageMetadata` but no `candidates`). The
/// adapter layer is responsible for emitting the trailing `data: [DONE]\n\n`
/// sentinel and for harvesting final usage separately — there is no Gemini
/// stream-end event analogous to Anthropic's `message_stop`.
pub fn translate_sse_line(line: &str) -> Option<Bytes> {
    let payload = line.strip_prefix("data: ")?;
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let text = v["candidates"]
        .get(0)?
        ["content"]["parts"]
        .as_array()?
        .iter()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let chunk = serde_json::json!({
        "id": "chatcmpl-vertex-stream",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    Some(Bytes::from(format!("data: {}\n\n", chunk)))
}

/// Stateful per-stream translator: forwards content chunks like
/// [`translate_sse_line`] while harvesting `usageMetadata` and `finishReason`
/// along the way, so the adapter can close the stream with a final chunk that
/// carries the provider's real token counts (issue #84). Which frame carries
/// `usageMetadata` varies by API version — sometimes the last content frame,
/// sometimes a trailing usage-only frame — so every frame is inspected and the
/// last value seen wins.
#[derive(Debug, Default)]
pub struct GeminiSseTranslator {
    prompt_tokens: u32,
    completion_tokens: u32,
    cached_tokens: u32,
    saw_usage: bool,
    finish_reason: Option<&'static str>,
}

impl GeminiSseTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one SSE line to an OpenAI-shaped content chunk, absorbing any
    /// usage or finish metadata the frame carries.
    pub fn translate_line(&mut self, line: &str) -> Option<Bytes> {
        let payload = line.strip_prefix("data: ")?;
        let v: serde_json::Value = serde_json::from_str(payload).ok()?;
        let usage = &v["usageMetadata"];
        if usage.is_object() {
            if let Some(n) = usage["promptTokenCount"].as_u64() {
                self.prompt_tokens = n as u32;
                self.saw_usage = true;
            }
            if let Some(n) = usage["candidatesTokenCount"].as_u64() {
                self.completion_tokens = n as u32;
                self.saw_usage = true;
            }
            if let Some(n) = usage["cachedContentTokenCount"].as_u64() {
                self.cached_tokens = n as u32;
            }
        }
        if let Some(r) = v["candidates"].get(0).and_then(|c| c["finishReason"].as_str()) {
            self.finish_reason = Some(map_finish_reason(r));
        }
        translate_sse_line(line)
    }

    /// The stream-terminating bytes: a finish chunk (with real usage when the
    /// provider reported it) followed by `data: [DONE]`. The adapter emits
    /// this once, after the upstream body ends.
    pub fn final_chunk(&self) -> Bytes {
        let mut chunk = serde_json::json!({
            "id": "chatcmpl-vertex-stream",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {}, "finish_reason": self.finish_reason.unwrap_or("stop")}]
        });
        if self.saw_usage {
            chunk["usage"] = serde_json::json!({
                "prompt_tokens": self.prompt_tokens,
                "completion_tokens": self.completion_tokens,
                "total_tokens": self.prompt_tokens + self.completion_tokens,
                "prompt_tokens_details": {"cached_tokens": self.cached_tokens}
            });
        }
        Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", chunk))
    }
}
