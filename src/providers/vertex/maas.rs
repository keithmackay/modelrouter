//! Vertex Model-as-a-Service (MaaS) publishers — Mistral, Llama, DeepSeek,
//! Qwen, AI21, OpenAI-on-Vertex, and whatever Model Garden adds next.
//!
//! These publishers are served through Vertex's OpenAI-compatible endpoint
//! (`…/locations/{region}/endpoints/openapi/chat/completions`) rather than the
//! per-publisher `:generateContent` / `:rawPredict` methods, and both the
//! request and the response are OpenAI chat-completions shaped. The `model`
//! field keeps its full `publisher/model` form — that is how the endpoint
//! distinguishes publishers.

use crate::providers::adapter::{CompletionResult, NormalizedRequest};

/// Build the OpenAI-compatible chat-completions body for a MaaS model.
/// `model` is the full `publisher/model` id (e.g. `mistralai/mistral-medium-3`).
pub fn translate_request(req: &NormalizedRequest, model: &str, streaming: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": req.messages,
        "stream": streaming,
    });
    if let Some(temp) = req.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(max) = req.max_tokens {
        body["max_tokens"] = serde_json::json!(max);
    }
    body
}

/// Parse the OpenAI-shaped MaaS response into the shared result type.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let choice = v["choices"]
        .get(0)
        .ok_or_else(|| anyhow::anyhow!("Vertex MaaS response has no choices"))?;
    let content = choice["message"]["content"].as_str().unwrap_or_default().to_string();
    let finish = choice["finish_reason"].as_str().unwrap_or("stop");
    let usage = &v["usage"];
    Ok(CompletionResult {
        content,
        prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0) as u32,
        finish_reason: finish.to_string(),
        cache_read_tokens: usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)
            as u32,
        cache_write_tokens: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_shaped_response() {
        let v = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1}
        });
        let r = parse_response(v).unwrap();
        assert_eq!(r.content, "hi");
        assert_eq!(r.prompt_tokens, 5);
        assert_eq!(r.completion_tokens, 1);
        assert_eq!(r.finish_reason, "stop");
    }

    #[test]
    fn request_keeps_full_publisher_model_id() {
        let req = NormalizedRequest {
            model: "vertex/mistralai/mistral-medium-3".into(),
            messages: vec![serde_json::json!({"role": "user", "content": "x"})],
            stream: false,
            temperature: Some(0.2),
            max_tokens: Some(100),
            extra_params: serde_json::Value::Null,
        };
        let body = translate_request(&req, "mistralai/mistral-medium-3", false);
        assert_eq!(body["model"], "mistralai/mistral-medium-3");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 100);
    }
}
