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
    Ok(CompletionResult {
        content,
        prompt_tokens: prompt,
        completion_tokens: completion,
        finish_reason: map_finish_reason(finish).to_string(),
    })
}

/// Translate a single Gemini SSE line to an OpenAI `chat.completion.chunk` line.
/// Returns `None` for comments, blank lines, or non-data events.
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
