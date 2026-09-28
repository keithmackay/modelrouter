use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::{ProviderConfig, TierTimeoutsConfig};
use crate::providers::adapter::{
    CompletionResult, EffectiveSettings, NormalizedRequest, ProviderAdapter, SseStream,
};

/// Anthropic requires `max_tokens`; sent when the caller supplies none.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicAdapter {
    api_key: String,
    /// Base for auxiliary endpoints (catalog); the messages path keeps its
    /// dedicated constant. Overridable via config.api_base (used by tests).
    api_base: String,
    client: reqwest::Client,
    /// This provider's configured flat timeout — the fallback `tier_timeouts`
    /// uses when a request's `request_model` doesn't name a known tier.
    default_timeout_secs: u64,
    tier_timeouts: TierTimeoutsConfig,
}

impl AnthropicAdapter {
    pub fn new(config: &ProviderConfig, tier_timeouts: TierTimeoutsConfig) -> Self {
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
            default_timeout_secs: config.timeout_secs,
            tier_timeouts,
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

/// Translate one OpenAI content part to its Anthropic-native block.
///
/// OpenAI `image_url` parts become Anthropic `image` blocks — Anthropic-family
/// backends reject the OpenAI tag with 400 `Input tag 'image_url' ... invalid`.
/// A data URL (`data:<media_type>;base64,<data>`) becomes a `base64` source;
/// any other URL becomes a `url` source. Every other part — text blocks,
/// already-native image blocks — passes through verbatim, and an `image_url`
/// part with no usable URL or a malformed data URL also passes through so the
/// provider's own error names the real problem.
fn translate_content_block(part: &serde_json::Value) -> serde_json::Value {
    if part["type"] != "image_url" {
        return part.clone();
    }
    // OpenAI nests the URL (`image_url: {url}`); some OpenAI-compatible
    // clients send the legacy flat string form (`image_url: "..."`).
    let url = part["image_url"]["url"]
        .as_str()
        .or_else(|| part["image_url"].as_str());
    let Some(url) = url else {
        return part.clone();
    };
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            if let Some(media_type) = meta.strip_suffix(";base64") {
                return serde_json::json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data},
                });
            }
        }
        // Malformed data URL: no comma, or not base64-encoded.
        return part.clone();
    }
    serde_json::json!({
        "type": "image",
        "source": {"type": "url", "url": url},
    })
}

/// Map an OpenAI content array to Anthropic-native blocks (see
/// [`translate_content_block`]). Identity for arrays with no `image_url`
/// parts, so text-only content stays byte-identical.
fn translate_content_blocks(parts: &[serde_json::Value]) -> Vec<serde_json::Value> {
    parts.iter().map(translate_content_block).collect()
}

/// Extract system messages (concatenated) and translate the rest to Anthropic
/// Messages shape, including the agentic tool loop (issue #88): an assistant
/// message carrying OpenAI `tool_calls` becomes `tool_use` content blocks, and
/// `tool`-role results become `tool_result` blocks in a user turn. Consecutive
/// tool results merge into one user turn — Anthropic wants every result for a
/// tool_use turn in the single user message that follows it.
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

    let mut translated: Vec<serde_json::Value> = Vec::new();
    for m in messages {
        match m["role"].as_str() {
            Some("user") => {
                if m["content"].is_string() {
                    translated.push(m.clone());
                } else if let Some(arr) = m["content"].as_array() {
                    let mut msg = m.clone();
                    msg["content"] = serde_json::Value::Array(translate_content_blocks(arr));
                    translated.push(msg);
                }
            }
            Some("assistant") => {
                let tool_calls = m["tool_calls"].as_array().filter(|t| !t.is_empty());
                if let Some(calls) = tool_calls {
                    // An OpenAI assistant turn with tool_calls usually has
                    // `content: null`; the old string/array filter dropped the
                    // whole turn, orphaning the tool results that follow.
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    if let Some(text) = m["content"].as_str() {
                        if !text.is_empty() {
                            blocks.push(serde_json::json!({"type": "text", "text": text}));
                        }
                    } else if let Some(arr) = m["content"].as_array() {
                        blocks.extend(translate_content_blocks(arr));
                    }
                    for call in calls {
                        // OpenAI carries arguments as a JSON *string*;
                        // Anthropic wants the object. Unparseable arguments
                        // become an empty input rather than a dropped call.
                        let input = call["function"]["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .filter(|v| v.is_object())
                            .unwrap_or_else(|| serde_json::json!({}));
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": call["id"],
                            "name": call["function"]["name"],
                            "input": input,
                        }));
                    }
                    translated.push(serde_json::json!({"role": "assistant", "content": blocks}));
                } else if m["content"].is_string() {
                    translated.push(m.clone());
                } else if let Some(arr) = m["content"].as_array() {
                    let mut msg = m.clone();
                    msg["content"] = serde_json::Value::Array(translate_content_blocks(arr));
                    translated.push(msg);
                }
            }
            Some("tool") => {
                let content = match &m["content"] {
                    serde_json::Value::String(s) => serde_json::Value::String(s.clone()),
                    serde_json::Value::Array(arr) => {
                        serde_json::Value::Array(translate_content_blocks(arr))
                    }
                    serde_json::Value::Null => serde_json::Value::String(String::new()),
                    other => serde_json::Value::String(other.to_string()),
                };
                let block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m["tool_call_id"],
                    "content": content,
                });
                let merged = match translated.last_mut() {
                    Some(prev)
                        if prev["role"] == "user"
                            && prev["content"].as_array().is_some_and(|arr| {
                                arr.iter().all(|b| b["type"] == "tool_result")
                            }) =>
                    {
                        prev["content"]
                            .as_array_mut()
                            .expect("checked as_array above")
                            .push(block.clone());
                        true
                    }
                    _ => false,
                };
                if !merged {
                    translated.push(serde_json::json!({"role": "user", "content": [block]}));
                }
            }
            _ => {}
        }
    }

    (system_text, translated)
}

/// OpenAI function tools → Anthropic tools (issue #88):
/// `{"type":"function","function":{name,description,parameters}}` →
/// `{name, description, input_schema}`. Entries without a name are skipped.
pub fn translate_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|t| {
            // Accept both the nested OpenAI shape and an already-flat one.
            let f = if t["function"].is_object() { &t["function"] } else { t };
            let name = f["name"].as_str()?;
            let schema = if f["parameters"].is_object() {
                f["parameters"].clone()
            } else {
                // Anthropic requires input_schema; a parameterless OpenAI
                // tool legally omits `parameters`.
                serde_json::json!({"type": "object", "properties": {}})
            };
            let mut out = serde_json::json!({"name": name, "input_schema": schema});
            if let Some(desc) = f["description"].as_str() {
                out["description"] = serde_json::json!(desc);
            }
            Some(out)
        })
        .collect()
}

/// OpenAI `tool_choice` → Anthropic `tool_choice` (issue #88). Returns `None`
/// for shapes with no Anthropic equivalent (the provider default, `auto`,
/// then applies).
pub fn translate_tool_choice(tc: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(s) = tc.as_str() {
        return match s {
            "auto" => Some(serde_json::json!({"type": "auto"})),
            "required" => Some(serde_json::json!({"type": "any"})),
            "none" => Some(serde_json::json!({"type": "none"})),
            _ => None,
        };
    }
    tc["function"]["name"]
        .as_str()
        .map(|name| serde_json::json!({"type": "tool", "name": name}))
}

/// Concatenated text blocks from an Anthropic `content` array.
pub fn text_from_content(content: &serde_json::Value) -> String {
    content
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Anthropic `tool_use` content blocks → OpenAI `tool_calls` array (issue #88).
/// `None` when the response called no tools.
pub fn tool_calls_from_content(content: &serde_json::Value) -> Option<serde_json::Value> {
    let calls: Vec<serde_json::Value> = content
        .as_array()?
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| {
            serde_json::json!({
                "id": b["id"],
                "type": "function",
                "function": {
                    "name": b["name"],
                    // OpenAI carries arguments as a JSON string.
                    "arguments": serde_json::to_string(&b["input"])
                        .unwrap_or_else(|_| "{}".to_string()),
                }
            })
        })
        .collect();
    if calls.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(calls))
    }
}

/// Map an Anthropic stop reason for the OpenAI surface. Only `tool_use` is
/// remapped — OpenAI clients dispatch their agentic loop on the literal string
/// `"tool_calls"`; other reasons keep their long-standing passthrough.
pub fn map_stop_reason(reason: &str) -> String {
    if reason == "tool_use" {
        "tool_calls".to_string()
    } else {
        reason.to_string()
    }
}

#[derive(serde::Deserialize)]
struct AnthropicResponse {
    /// Raw content blocks: text is joined, `tool_use` blocks translate to
    /// OpenAI `tool_calls` (issue #88).
    content: serde_json::Value,
    usage: AnthropicUsage,
    stop_reason: Option<String>,
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

/// Fold the request's OpenAI-shaped tools into an Anthropic body (issue #88).
/// `tool_choice` only rides along with tools — Anthropic rejects it alone.
fn apply_tools(body: &mut serde_json::Value, req: &NormalizedRequest) {
    let Some(tools) = &req.tools else { return };
    body["tools"] = serde_json::Value::Array(translate_tools(tools));
    if let Some(tc) = req.tool_choice.as_ref().and_then(translate_tool_choice) {
        body["tool_choice"] = tc;
    }
}

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
            body["max_tokens"] = serde_json::json!(DEFAULT_MAX_TOKENS);
        }
        apply_tools(&mut body, req);

        let timeout_secs = self.tier_timeouts.resolve(&req.request_model, self.default_timeout_secs);
        let dispatched = std::time::Instant::now();
        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Anthropic")?;
        // Headers are in, body not yet read: time to first token.
        let ttft_ms = dispatched.elapsed().as_millis() as i64;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic returned {}: {}", status, text);
        }

        let parsed: AnthropicResponse = resp
            .json()
            .await
            .context("Failed to parse Anthropic response")?;

        Ok(CompletionResult {
            content: text_from_content(&parsed.content),
            prompt_tokens: parsed.usage.input_tokens,
            completion_tokens: parsed.usage.output_tokens,
            finish_reason: map_stop_reason(
                parsed.stop_reason.as_deref().unwrap_or("end_turn"),
            ),
            cache_read_tokens: parsed.usage.cache_read_input_tokens,
            cache_write_tokens: parsed.usage.cache_creation_input_tokens,
            reasoning_tokens: None,
            ttft_ms: Some(ttft_ms),
            tool_calls: tool_calls_from_content(&parsed.content),
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
            body["max_tokens"] = serde_json::json!(DEFAULT_MAX_TOKENS);
        }
        apply_tools(&mut body, req);

        let timeout_secs = self.tier_timeouts.resolve(&req.request_model, self.default_timeout_secs);
        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(timeout_secs))
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

    /// Claude models take tools natively; the adapter translates the OpenAI
    /// shape both ways (issue #88).
    fn supports_tools(&self, _model: &str) -> bool {
        true
    }

    /// `max_tokens` is always sent (Anthropic requires it): the caller's value
    /// or [`DEFAULT_MAX_TOKENS`]. The timeout is the tier-resolved ceiling.
    fn effective_settings(&self, req: &NormalizedRequest) -> EffectiveSettings {
        EffectiveSettings {
            temperature: req.temperature,
            max_tokens: Some(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
            timeout_secs: Some(
                self.tier_timeouts
                    .resolve(&req.request_model, self.default_timeout_secs),
            ),
        }
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
    /// Anthropic content-block index → OpenAI tool_call index (issue #88).
    /// Anthropic numbers ALL content blocks (text included); OpenAI numbers
    /// only the tool calls, so the two drift apart as soon as a text block
    /// precedes a tool_use block.
    tool_indices: std::collections::HashMap<u64, u64>,
    next_tool_index: u64,
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
            "content_block_start" => {
                // A tool_use block opens: emit the OpenAI tool_call header
                // chunk (id + name, empty arguments) so the client can start
                // accumulating (issue #88). Text blocks carry nothing here.
                let block = &v["content_block"];
                if block["type"] != "tool_use" {
                    return None;
                }
                let anthropic_index = v["index"].as_u64().unwrap_or(0);
                let tool_index = self.next_tool_index;
                self.next_tool_index += 1;
                self.tool_indices.insert(anthropic_index, tool_index);
                let chunk = serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": tool_index,
                        "id": block["id"],
                        "type": "function",
                        "function": {"name": block["name"], "arguments": ""}
                    }]}, "finish_reason": null}]
                });
                Some(Bytes::from(format!("data: {}\n\n", chunk)))
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
                } else if v["delta"]["type"] == "input_json_delta" {
                    // Partial tool arguments stream as OpenAI argument
                    // fragments against the tool index opened above (issue #88).
                    let anthropic_index = v["index"].as_u64().unwrap_or(0);
                    let tool_index = *self.tool_indices.get(&anthropic_index)?;
                    let partial = v["delta"]["partial_json"].as_str()?;
                    let chunk = serde_json::json!({
                        "id": "chatcmpl-stream",
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"tool_calls": [{
                            "index": tool_index,
                            "function": {"arguments": partial}
                        }]}, "finish_reason": null}]
                    });
                    Some(Bytes::from(format!("data: {}\n\n", chunk)))
                } else {
                    None
                }
            }
            "message_delta" => {
                self.absorb_usage(&v["usage"]);
                // OpenAI agentic clients dispatch on the literal string
                // "tool_calls" (issue #88); every other stop reason keeps the
                // long-standing "stop".
                let finish_reason = match v["delta"]["stop_reason"].as_str() {
                    Some("tool_use") => "tool_calls",
                    _ => "stop",
                };
                let mut chunk = serde_json::json!({
                    "id": "chatcmpl-stream",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
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
        // `tool` is no longer an unknown role (issue #88) — it translates to a
        // tool_result turn, covered separately below. Legacy `function` is
        // still dropped.
        let messages = vec![
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

    #[test]
    fn assistant_tool_calls_become_tool_use_blocks(/* issue #88 */) {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "What's the weather in Oslo?"}),
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Oslo\"}"}
                }]
            }),
        ];
        let (_, translated) = translate_messages(&messages);
        // The old string/array filter dropped the whole assistant turn.
        assert_eq!(translated.len(), 2);
        let blocks = translated[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_1");
        assert_eq!(blocks[0]["name"], "get_weather");
        assert_eq!(blocks[0]["input"]["city"], "Oslo");
    }

    #[test]
    fn assistant_text_plus_tool_calls_keeps_both(/* issue #88 */) {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": "Let me check.",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "lookup", "arguments": "{}"}
            }]
        })];
        let (_, translated) = translate_messages(&messages);
        let blocks = translated[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Let me check.");
        assert_eq!(blocks[1]["type"], "tool_use");
    }

    #[test]
    fn unparseable_tool_arguments_become_empty_input(/* issue #88 */) {
        let messages = vec![serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "lookup", "arguments": "not json"}
            }]
        })];
        let (_, translated) = translate_messages(&messages);
        let blocks = translated[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["input"], serde_json::json!({}));
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn(/* issue #88 */) {
        let messages = vec![
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "b", "arguments": "{}"}}
                ]
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "call_1", "content": "result 1"}),
            serde_json::json!({"role": "tool", "tool_call_id": "call_2", "content": "result 2"}),
            serde_json::json!({"role": "user", "content": "thanks"}),
        ];
        let (_, translated) = translate_messages(&messages);
        // assistant turn, ONE merged tool_result user turn, trailing user turn
        assert_eq!(translated.len(), 3);
        assert_eq!(translated[1]["role"], "user");
        let results = translated[1]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "call_1");
        assert_eq!(results[0]["content"], "result 1");
        assert_eq!(results[1]["tool_use_id"], "call_2");
        assert_eq!(translated[2]["content"], "thanks");
    }
}

#[cfg(test)]
mod image_content_tests {
    // OpenAI `image_url` content parts must translate to Anthropic-native
    // `image` blocks — Anthropic-family backends reject the OpenAI tag with
    // 400 "Input tag 'image_url' ... invalid".
    use super::translate_messages;

    #[test]
    fn string_content_is_byte_identical() {
        let messages = vec![serde_json::json!({"role": "user", "content": "Hello"})];
        let (_, translated) = translate_messages(&messages);
        assert_eq!(translated[0], messages[0]);
    }

    #[test]
    fn text_only_content_arrays_are_byte_identical() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "part one"},
                {"type": "text", "text": "part two"}
            ]
        })];
        let (_, translated) = translate_messages(&messages);
        assert_eq!(translated[0], messages[0]);
    }

    #[test]
    fn data_url_image_becomes_base64_image_block() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
            }]
        })];
        let (_, translated) = translate_messages(&messages);
        let block = &translated[0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], "iVBORw0KGgo=");
        assert!(block.get("image_url").is_none());
    }

    #[test]
    fn https_image_url_becomes_url_source_block() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": {"url": "https://example.com/diagram.png"}
            }]
        })];
        let (_, translated) = translate_messages(&messages);
        let block = &translated[0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://example.com/diagram.png");
    }

    #[test]
    fn legacy_flat_string_image_url_is_translated() {
        // Some OpenAI-compatible clients send `image_url` as a bare string
        // instead of the nested `{url}` object.
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": "https://example.com/a.jpg"}]
        })];
        let (_, translated) = translate_messages(&messages);
        let block = &translated[0]["content"][0];
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://example.com/a.jpg");
    }

    #[test]
    fn mixed_text_and_image_parts_keep_order_and_text_untouched() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "What does this show?"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}},
                {"type": "text", "text": "Answer briefly."}
            ]
        })];
        let (_, translated) = translate_messages(&messages);
        let blocks = translated[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], messages[0]["content"][0]);
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(blocks[2], messages[0]["content"][2]);
    }

    #[test]
    fn malformed_data_url_passes_through_without_panic() {
        // No comma separator, and a non-base64 encoding marker: neither can
        // become a valid Anthropic block, so both pass through verbatim and
        // the provider's own error names the real problem.
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64"}},
                {"type": "image_url", "image_url": {"url": "data:image/png;utf8,notbase64"}}
            ]
        })];
        let (_, translated) = translate_messages(&messages);
        let blocks = translated[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0], messages[0]["content"][0]);
        assert_eq!(blocks[1], messages[0]["content"][1]);
    }

    #[test]
    fn image_url_part_without_a_url_passes_through() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [{"type": "image_url", "image_url": {}}]
        })];
        let (_, translated) = translate_messages(&messages);
        assert_eq!(translated[0], messages[0]);
    }

    #[test]
    fn native_anthropic_image_blocks_pass_through_unchanged() {
        let messages = vec![serde_json::json!({
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
            }]
        })];
        let (_, translated) = translate_messages(&messages);
        assert_eq!(translated[0], messages[0]);
    }

    #[test]
    fn tool_result_content_arrays_are_translated_too() {
        // Anthropic tool_result blocks accept image blocks, not `image_url`.
        let messages = vec![serde_json::json!({
            "role": "tool",
            "tool_call_id": "call_1",
            "content": [
                {"type": "text", "text": "screenshot:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]
        })];
        let (_, translated) = translate_messages(&messages);
        let result = &translated[0]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["content"][0], messages[0]["content"][0]);
        assert_eq!(result["content"][1]["type"], "image");
        assert_eq!(result["content"][1]["source"]["data"], "AAAA");
    }
}

#[cfg(test)]
mod tool_translation_tests {
    use super::{
        map_stop_reason, text_from_content, tool_calls_from_content, translate_tool_choice,
        translate_tools,
    };

    #[test]
    fn openai_function_tools_translate_to_anthropic_shape(/* issue #88 */) {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        })];
        let out = translate_tools(&tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "get_weather");
        assert_eq!(out[0]["description"], "Get the weather");
        assert_eq!(out[0]["input_schema"]["properties"]["city"]["type"], "string");
        assert!(out[0].get("parameters").is_none());
    }

    #[test]
    fn parameterless_tool_gets_empty_object_schema(/* issue #88 */) {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "ping"}
        })];
        let out = translate_tools(&tools);
        assert_eq!(out[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn nameless_tool_entries_are_skipped(/* issue #88 */) {
        let tools = vec![serde_json::json!({"type": "function", "function": {}})];
        assert!(translate_tools(&tools).is_empty());
    }

    #[test]
    fn tool_choice_strings_map_to_anthropic_modes(/* issue #88 */) {
        assert_eq!(
            translate_tool_choice(&serde_json::json!("auto")).unwrap()["type"],
            "auto"
        );
        assert_eq!(
            translate_tool_choice(&serde_json::json!("required")).unwrap()["type"],
            "any"
        );
        assert_eq!(
            translate_tool_choice(&serde_json::json!("none")).unwrap()["type"],
            "none"
        );
        assert!(translate_tool_choice(&serde_json::json!("bogus")).is_none());
    }

    #[test]
    fn named_function_tool_choice_pins_the_tool(/* issue #88 */) {
        let tc = serde_json::json!({"type": "function", "function": {"name": "get_weather"}});
        let out = translate_tool_choice(&tc).unwrap();
        assert_eq!(out["type"], "tool");
        assert_eq!(out["name"], "get_weather");
    }

    #[test]
    fn tool_use_blocks_translate_to_openai_tool_calls(/* issue #88 */) {
        let content = serde_json::json!([
            {"type": "text", "text": "Checking."},
            {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Oslo"}}
        ]);
        assert_eq!(text_from_content(&content), "Checking.");
        let calls = tool_calls_from_content(&content).unwrap();
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        // Arguments are a JSON *string* on the OpenAI surface.
        let args: serde_json::Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], "Oslo");
    }

    #[test]
    fn no_tool_use_blocks_means_no_tool_calls(/* issue #88 */) {
        let content = serde_json::json!([{"type": "text", "text": "hi"}]);
        assert!(tool_calls_from_content(&content).is_none());
    }

    #[test]
    fn stop_reason_tool_use_maps_to_tool_calls(/* issue #88 */) {
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
        // Long-standing passthrough for everything else is preserved.
        assert_eq!(map_stop_reason("end_turn"), "end_turn");
        assert_eq!(map_stop_reason("max_tokens"), "max_tokens");
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
    fn tool_use_blocks_stream_as_openai_tool_call_deltas(/* issue #88 */) {
        let mut t = AnthropicSseTranslator::new();
        let out = lines(
            &mut t,
            &[
                // Text block at anthropic index 0, tool_use at index 1: the
                // OpenAI tool index must be 0, not 1.
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking."}}"#,
                r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Oslo\"}"}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            ],
        );
        let chunks: Vec<serde_json::Value> = out
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        // chunk 0: text; chunk 1: tool_call header; 2-3: argument fragments; 4: final
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Checking.");
        let header = &chunks[1]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(header["index"], 0);
        assert_eq!(header["id"], "toolu_1");
        assert_eq!(header["type"], "function");
        assert_eq!(header["function"]["name"], "get_weather");
        assert_eq!(header["function"]["arguments"], "");
        let frag1 = &chunks[2]["choices"][0]["delta"]["tool_calls"][0];
        let frag2 = &chunks[3]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(frag1["index"], 0);
        let assembled = format!(
            "{}{}",
            frag1["function"]["arguments"].as_str().unwrap(),
            frag2["function"]["arguments"].as_str().unwrap()
        );
        let args: serde_json::Value = serde_json::from_str(&assembled).unwrap();
        assert_eq!(args["city"], "Oslo");
        assert_eq!(chunks[4]["choices"][0]["finish_reason"], "tool_calls");
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn non_tool_stop_reason_still_finishes_with_stop(/* issue #88 */) {
        let mut t = AnthropicSseTranslator::new();
        let out = lines(
            &mut t,
            &[r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#],
        );
        let v: serde_json::Value = serde_json::from_str(
            out.lines().find_map(|l| l.strip_prefix("data: ")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
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
        let models = AnthropicAdapter::new(&config, crate::config::schema::TierTimeoutsConfig::default())
            .list_models()
            .await
            .unwrap();
        assert_eq!(models[0].provider, "anthropic");
        assert_eq!(models[0].name, "claude-sonnet-4-5");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 4.5"));
    }
}
