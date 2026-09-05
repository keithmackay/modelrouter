use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

pub struct AnthropicAdapter {
    api_key: String,
    /// Base for auxiliary endpoints (catalog); the messages path keeps its
    /// dedicated constant. Overridable via config.api_base (used by tests).
    api_base: String,
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
            api_base: config
                .api_base
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            client,
        }
    }
}

// ── Catalog discovery (issue #33) ────────────────────────────────────────────

#[async_trait::async_trait]
impl crate::providers::catalog::ProviderCatalog for AnthropicAdapter {
    /// GET {api_base}/models with the standard Anthropic headers.
    async fn list_models(&self) -> anyhow::Result<Vec<crate::providers::catalog::CatalogModel>> {
        let url = format!("{}/models", self.api_base);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("catalog request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("catalog returned {status}: {text}");
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                m["id"].as_str().map(|id| crate::providers::catalog::CatalogModel {
                    provider: "anthropic".to_string(),
                    name: id.to_string(),
                    display_name: m["display_name"].as_str().map(str::to_string),
                })
            })
            .collect())
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
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
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
            cache_read_tokens: parsed.usage.cache_read_input_tokens,
            cache_write_tokens: parsed.usage.cache_creation_input_tokens,
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

        // One translator per stream: usage arrives split across events
        // (`message_start` carries input tokens, `message_delta` output
        // tokens), so the final OpenAI chunk needs state from earlier lines.
        let mut translator = AnthropicSseTranslator::new();
        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("Stream error: {}", e))
            .map_ok(move |chunk| {
                // Translate Anthropic SSE lines to OpenAI-compatible format
                let text = String::from_utf8_lossy(&chunk);
                let mut out = String::new();
                for line in text.lines() {
                    if let Some(translated) = translator.translate_line(line) {
                        out.push_str(&String::from_utf8_lossy(&translated));
                    }
                }
                Bytes::from(out)
            });

        Ok(Box::pin(stream))
    }
}

/// Translates one Anthropic SSE stream into OpenAI-shaped chunks.
///
/// Stateful because Anthropic reports usage in two places: `message_start`
/// carries `input_tokens` (and `cache_read_input_tokens`), `message_delta`
/// carries `output_tokens`. Both are folded into a `usage` object on the final
/// chunk — the shape OpenAI emits with `stream_options.include_usage` — so the
/// streaming ledger can record what the provider counted rather than an
/// estimate.
#[derive(Debug, Default)]
pub struct AnthropicSseTranslator {
    input_tokens: u32,
    cache_read_input_tokens: u32,
    output_tokens: u32,
    /// True once any `usage` object has been seen; without one the final chunk
    /// carries no `usage` and the ledger falls back to its estimate.
    saw_usage: bool,
}

impl AnthropicSseTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    fn absorb_usage(&mut self, usage: &serde_json::Value) {
        if !usage.is_object() {
            return;
        }
        self.saw_usage = true;
        if let Some(n) = usage["input_tokens"].as_u64() {
            self.input_tokens = n as u32;
        }
        if let Some(n) = usage["cache_read_input_tokens"].as_u64() {
            self.cache_read_input_tokens = n as u32;
        }
        if let Some(n) = usage["output_tokens"].as_u64() {
            self.output_tokens = n as u32;
        }
    }

    /// Translate a single SSE line. Returns the OpenAI-shaped bytes to forward,
    /// or `None` for lines that carry nothing the client needs (event names,
    /// pings, block boundaries).
    pub fn translate_line(&mut self, line: &str) -> Option<Bytes> {
        if !line.starts_with("data: ") {
            return None;
        }
        let json_str = &line["data: ".len()..];
        let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
        match v["type"].as_str()? {
            "message_start" => {
                self.absorb_usage(&v["message"]["usage"]);
                None
            }
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
                self.absorb_usage(&v["usage"]);
                let mut chunk = serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                });
                if self.saw_usage {
                    // OpenAI's `prompt_tokens` is the whole prompt, cached
                    // tokens included; `cached_tokens` names the subset.
                    let prompt_tokens = self.input_tokens + self.cache_read_input_tokens;
                    chunk["usage"] = serde_json::json!({
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": self.output_tokens,
                        "total_tokens": prompt_tokens + self.output_tokens,
                        "prompt_tokens_details": {
                            "cached_tokens": self.cache_read_input_tokens
                        }
                    });
                }
                let done = "data: [DONE]\n\n";
                Some(Bytes::from(format!("data: {}\n\n{}", chunk, done)))
            }
            _ => None,
        }
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

#[cfg(test)]
mod sse_translator_tests {
    use super::AnthropicSseTranslator;

    fn lines(t: &mut AnthropicSseTranslator, raw: &[&str]) -> String {
        raw.iter()
            .filter_map(|l| t.translate_line(l))
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .collect()
    }

    #[test]
    fn folds_message_start_and_delta_usage_into_final_chunk() {
        let mut t = AnthropicSseTranslator::new();
        let out = lines(
            &mut t,
            &[
                "event: message_start",
                r#"data: {"type":"message_start","message":{"usage":{"input_tokens":40,"cache_read_input_tokens":10,"output_tokens":1}}}"#,
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
                r#"data: {"type":"message_stop"}"#,
            ],
        );
        let final_chunk = out
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .rfind(|d| *d != "[DONE]")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(final_chunk).unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["prompt_tokens"], 50);
        assert_eq!(v["usage"]["completion_tokens"], 7);
        assert_eq!(v["usage"]["total_tokens"], 57);
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 10);
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn text_deltas_pass_through_and_omit_usage_when_none_was_reported() {
        let mut t = AnthropicSseTranslator::new();
        let out = lines(
            &mut t,
            &[
                r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ],
        );
        let chunks: Vec<serde_json::Value> = out
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Hello");
        assert!(chunks[1].get("usage").is_none());
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::providers::catalog::ProviderCatalog;
    use axum::{routing::get, Json, Router};

    #[tokio::test]
    async fn lists_models_with_display_names() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let router = Router::new().route(
                "/models",
                get(|| async {
                    Json(serde_json::json!({"data": [
                        {"id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5"}
                    ]}))
                }),
            );
            axum::serve(listener, router).await.unwrap()
        });
        let mut config = crate::config::schema::ProviderConfig::default();
        config.api_base = Some(format!("http://{addr}"));
        config.api_key = "k".into();
        let models = AnthropicAdapter::new(&config).list_models().await.unwrap();
        assert_eq!(models[0].provider, "anthropic");
        assert_eq!(models[0].name, "claude-sonnet-4-5");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 4.5"));
    }
}
