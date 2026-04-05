use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

pub struct AnthropicAdapter {
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to build reqwest client");
        Self {
            api_key: config.api_key.clone(),
            client,
        }
    }
}

/// Extract system messages (concatenated) and filter to user/assistant roles only.
pub fn translate_messages(
    messages: &[serde_json::Value],
) -> (Option<String>, Vec<serde_json::Value>) {
    let system_parts: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            if m["role"].as_str() != Some("system") {
                return None;
            }
            if let Some(s) = m["content"].as_str() {
                return Some(s.to_string());
            }
            if let Some(arr) = m["content"].as_array() {
                let text = arr
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() { Some(text) } else { None }
            } else {
                None
            }
        })
        .collect();

    let system_text = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n"))
    };

    let filtered: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| {
            matches!(m["role"].as_str(), Some("user") | Some("assistant"))
        })
        .filter(|m| m["content"].is_string() || m["content"].is_array())
        .cloned()
        .collect();

    (system_text, filtered)
}

#[derive(serde::Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    usage: AnthropicUsage,
    stop_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(serde::Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[async_trait::async_trait]
impl ProviderAdapter for AnthropicAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let (system_text, messages) = translate_messages(&req.messages);

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": false,
        });

        if let Some(system) = system_text {
            body["system"] = serde_json::json!(system);
        }
        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        } else {
            // Anthropic requires max_tokens
            body["max_tokens"] = serde_json::json!(4096);
        }

        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic returned {}: {}", status, text);
        }

        let parsed: AnthropicResponse = resp
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

        let content = parsed
            .content
            .into_iter()
            .filter(|c| c.content_type == "text")
            .filter_map(|c| c.text)
            .collect::<Vec<_>>()
            .join("");

        Ok(CompletionResult {
            content,
            prompt_tokens: parsed.usage.input_tokens,
            completion_tokens: parsed.usage.output_tokens,
            finish_reason: parsed.stop_reason.unwrap_or_else(|| "end_turn".to_string()),
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let (system_text, messages) = translate_messages(&req.messages);

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
        });

        if let Some(system) = system_text {
            body["system"] = serde_json::json!(system);
        }
        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        } else {
            body["max_tokens"] = serde_json::json!(4096);
        }

        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send streaming request to Anthropic")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic returned {}: {}", status, text);
        }

        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("Stream error: {}", e))
            .map_ok(|chunk| {
                // Translate Anthropic SSE lines to OpenAI-compatible format
                let text = String::from_utf8_lossy(&chunk);
                let mut out = String::new();
                for line in text.lines() {
                    if let Some(translated) = translate_anthropic_sse(line) {
                        out.push_str(&String::from_utf8_lossy(&translated));
                    }
                }
                Bytes::from(out)
            });

        Ok(Box::pin(stream))
    }
}

fn translate_anthropic_sse(line: &str) -> Option<Bytes> {
    if !line.starts_with("data: ") {
        return None;
    }
    let json_str = &line["data: ".len()..];
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    match v["type"].as_str()? {
        "content_block_delta" => {
            if v["delta"]["type"] == "text_delta" {
                let text = v["delta"]["text"].as_str()?;
                let chunk = serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                });
                Some(Bytes::from(format!("data: {}\n\n", chunk)))
            } else {
                None
            }
        }
        "message_delta" => {
            let chunk = serde_json::json!({
                "id": "chatcmpl-stream",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            let done = "data: [DONE]\n\n";
            Some(Bytes::from(format!("data: {}\n\n{}", chunk, done)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::translate_messages;

    #[test]
    fn translate_no_system_message() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "Hello"}),
        ];
        let (system, filtered) = translate_messages(&messages);
        assert!(system.is_none());
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn translate_single_system_message() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "You are a helpful assistant."}),
            serde_json::json!({"role": "user", "content": "Hello"}),
        ];
        let (system, filtered) = translate_messages(&messages);
        assert_eq!(system.as_deref(), Some("You are a helpful assistant."));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["role"], "user");
    }

    #[test]
    fn translate_multiple_system_messages() {
        let messages = vec![
            serde_json::json!({"role": "system", "content": "Part 1."}),
            serde_json::json!({"role": "system", "content": "Part 2."}),
            serde_json::json!({"role": "user", "content": "Hello"}),
        ];
        let (system, filtered) = translate_messages(&messages);
        assert_eq!(system.as_deref(), Some("Part 1.\nPart 2."));
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn translate_unknown_roles_filtered_out() {
        let messages = vec![
            serde_json::json!({"role": "tool", "content": "tool output"}),
            serde_json::json!({"role": "user", "content": "Hello"}),
            serde_json::json!({"role": "function", "content": "func result"}),
            serde_json::json!({"role": "assistant", "content": "Hi there"}),
        ];
        let (system, filtered) = translate_messages(&messages);
        assert!(system.is_none());
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0]["role"], "user");
        assert_eq!(filtered[1]["role"], "assistant");
    }
}
