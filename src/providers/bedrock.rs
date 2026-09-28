// src/providers/bedrock.rs
//
// AWS Bedrock Converse API adapter.
//
// Credentials: standard AWS chain (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY /
// AWS_SESSION_TOKEN env vars, or ~/.aws/credentials + ~/.aws/config).
// Region: ProviderConfig.region, falling back to AWS_REGION env var / ~/.aws/config.
//
// Streaming limitation: the current `stream()` implementation collects all events
// before returning them. This means the HTTP response body does not begin sending
// until Bedrock finishes generating. Progressive streaming requires async-stream
// or a tokio channel and is left as a follow-up task.

use anyhow::Context;
use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::config::Region;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock,
};
use bytes::Bytes;
use futures::stream;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{
    CompletionResult, EffectiveSettings, NormalizedRequest, ProviderAdapter, SseStream,
};

// ── Translation helpers (pub so unit tests can import them) ─────────────────

/// Convert OpenAI-format messages to Bedrock Converse `messages` JSON array.
/// System messages are excluded — use `build_system_prompt` for those.
/// Returns `serde_json::Value` for easy unit testing; SDK types are built separately.
pub fn build_converse_messages(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| {
            let text = m["content"].as_str().unwrap_or("").to_string();
            serde_json::json!({
                "role": m["role"],
                "content": [{"text": text}],
            })
        })
        .collect()
}

/// Extract system messages into Bedrock `system` JSON array format.
pub fn build_system_prompt(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| {
            let text = m["content"].as_str().unwrap_or("").to_string();
            serde_json::json!({"text": text})
        })
        .collect()
}

/// Build a JSON `inferenceConfig` object. Used in unit tests and as reference.
/// Keys are omitted when None.
pub fn build_inference_config(
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(t) = temperature {
        map.insert("temperature".into(), serde_json::json!(t));
    }
    if let Some(m) = max_tokens {
        map.insert("maxTokens".into(), serde_json::json!(m));
    }
    serde_json::Value::Object(map)
}

// ── SDK type builders (not exported; used by complete/stream) ────────────────

/// Convert OpenAI-format messages into AWS SDK `Message` types using builder APIs.
/// AWS SDK types do NOT implement serde::Deserialize — JSON round-trip will not compile.
fn to_sdk_messages(messages: &[serde_json::Value]) -> anyhow::Result<Vec<Message>> {
    messages
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| {
            let role = match m["role"].as_str().unwrap_or("user") {
                "assistant" => ConversationRole::Assistant,
                _ => ConversationRole::User,
            };
            let text = m["content"].as_str().unwrap_or("").to_string();
            Message::builder()
                .role(role)
                .content(ContentBlock::Text(text))
                .build()
                .context("Failed to build Bedrock Message")
        })
        .collect()
}

/// Convert system messages into AWS SDK `SystemContentBlock` types.
fn to_sdk_system(messages: &[serde_json::Value]) -> Vec<SystemContentBlock> {
    messages
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| {
            let text = m["content"].as_str().unwrap_or("").to_string();
            SystemContentBlock::Text(text)
        })
        .collect()
}

// ── Adapter ──────────────────────────────────────────────────────────────────

pub struct BedrockAdapter {
    client: aws_sdk_bedrockruntime::Client,
    /// The SDK operation timeout, reported in `x_router.settings`.
    timeout_secs: u64,
}

impl BedrockAdapter {
    /// Construct a BedrockAdapter. This is async because aws_config::load is async.
    /// Called from ProviderRegistry::get via block_in_place + block_on.
    pub async fn new(config: &ProviderConfig) -> Self {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = &config.region {
            // Region lives in aws_sdk_bedrockruntime::config::Region (re-exported from aws-types)
            loader = loader.region(Region::new(region.clone()));
        }
        // Previously unset entirely, so every call ran under the AWS SDK's
        // own defaults — which the SDK does not name as a stable contract,
        // and which can differ silently across SDK versions. `config.timeout_secs`
        // (already read by every other provider adapter) now applies here too,
        // as an explicit ceiling on both connect and the overall operation.
        let timeout_config = aws_config::timeout::TimeoutConfig::builder()
            .connect_timeout(std::time::Duration::from_secs(config.timeout_secs))
            .operation_timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build();
        loader = loader.timeout_config(timeout_config);
        let aws_config = loader.load().await;
        let client = aws_sdk_bedrockruntime::Client::new(&aws_config);
        Self {
            client,
            timeout_secs: config.timeout_secs,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for BedrockAdapter {
    /// Settings forwarded as normalized; the timeout is the configured ceiling.
    fn effective_settings(&self, req: &NormalizedRequest) -> EffectiveSettings {
        EffectiveSettings {
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            timeout_secs: Some(self.timeout_secs),
        }
    }

    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let sdk_messages = to_sdk_messages(&req.messages)?;
        let sdk_system = to_sdk_system(&req.messages);

        let mut builder = self
            .client
            .converse()
            .model_id(&req.model)
            .set_messages(Some(sdk_messages));

        if !sdk_system.is_empty() {
            builder = builder.set_system(Some(sdk_system));
        }

        // Build InferenceConfiguration with optional fields
        let mut inf_builder = InferenceConfiguration::builder();
        if let Some(t) = req.temperature {
            inf_builder = inf_builder.temperature(t as f32);
        }
        if let Some(m) = req.max_tokens {
            inf_builder = inf_builder.max_tokens(m as i32);
        }
        builder = builder.inference_config(inf_builder.build());

        let resp = builder
            .send()
            .await
            .context("Bedrock converse request failed")?;

        // Extract text from first content block of response message
        let content = resp
            .output()
            .and_then(|o| o.as_message().ok())
            .and_then(|m| m.content().first())
            .and_then(|b| b.as_text().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();

        // stop_reason() returns &StopReason (non-optional) — use as_str() directly
        let finish_reason = resp.stop_reason().as_str().to_string();

        // SDK returns i32 for token counts; clamp negatives to 0 defensively
        let (prompt_tokens, completion_tokens) = resp
            .usage()
            .map(|u| {
                (
                    u.input_tokens().max(0) as u32,
                    u.output_tokens().max(0) as u32,
                )
            })
            .unwrap_or((0, 0));

        Ok(CompletionResult {
            content,
            prompt_tokens,
            completion_tokens,
            finish_reason,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
            // The AWS SDK's converse() resolves only once the whole response
            // is in — there is no header/body split to time, so no TTFT.
            ttft_ms: None,
            tool_calls: None,
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        // Known limitation: this implementation collects all Bedrock events before
        // returning the stream. The HTTP response will not start sending until
        // Bedrock finishes. Progressive streaming requires async-stream or a tokio
        // channel and is left as a follow-up task.
        let sdk_messages = to_sdk_messages(&req.messages)?;
        let sdk_system = to_sdk_system(&req.messages);

        let mut builder = self
            .client
            .converse_stream()
            .model_id(&req.model)
            .set_messages(Some(sdk_messages));

        if !sdk_system.is_empty() {
            builder = builder.set_system(Some(sdk_system));
        }

        let mut inf_builder = InferenceConfiguration::builder();
        if let Some(t) = req.temperature {
            inf_builder = inf_builder.temperature(t as f32);
        }
        if let Some(m) = req.max_tokens {
            inf_builder = inf_builder.max_tokens(m as i32);
        }
        builder = builder.inference_config(inf_builder.build());

        let mut event_stream = builder
            .send()
            .await
            .context("Bedrock converse_stream request failed")?
            .stream;

        let mut chunks: Vec<anyhow::Result<Bytes>> = Vec::new();
        // The stream's Metadata event carries what Bedrock actually metered.
        // Harvest it so the finish chunk reports real token counts and the
        // streaming ledger doesn't fall back to estimates (issue #84).
        let mut usage: Option<(u32, u32)> = None;

        loop {
            match event_stream.recv().await {
                Ok(Some(event)) => {
                    if let aws_sdk_bedrockruntime::types::ConverseStreamOutput::Metadata(m) = &event {
                        if let Some(u) = m.usage() {
                            usage = Some((u.input_tokens().max(0) as u32, u.output_tokens().max(0) as u32));
                        }
                    }
                    if let Some(chunk) = process_stream_event(event) {
                        chunks.push(Ok(chunk));
                    }
                    // Other non-text events (message start/stop) are silently skipped.
                }
                Ok(None) => break,
                Err(e) => {
                    chunks.push(Err(anyhow::anyhow!("Bedrock stream error: {}", e)));
                    break;
                }
            }
        }

        // Only emit the finish chunk and [DONE] on clean end-of-stream — not
        // after an error event.
        let had_error = chunks.iter().any(|c| c.is_err());
        if !had_error {
            let mut chunk = serde_json::json!({
                "id": "chatcmpl-bedrock-stream",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            if let Some((prompt, completion)) = usage {
                chunk["usage"] = serde_json::json!({
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "total_tokens": prompt + completion,
                });
            }
            chunks.push(Ok(Bytes::from(format!("data: {}\n\n", chunk))));
            chunks.push(Ok(Bytes::from("data: [DONE]\n\n")));
        }
        Ok(Box::pin(stream::iter(chunks)))
    }
}

/// Convert a single Bedrock stream event to an SSE chunk if it contains text.
fn process_stream_event(event: aws_sdk_bedrockruntime::types::ConverseStreamOutput) -> Option<Bytes> {
    use aws_sdk_bedrockruntime::types::ConverseStreamOutput;

    if let ConverseStreamOutput::ContentBlockDelta(delta_event) = event {
        if let Some(delta) = delta_event.delta() {
            if let Ok(text) = delta.as_text() {
                let sse = format!(
                    "data: {}\n\n",
                    serde_json::json!({
                        "choices": [{
                            "delta": {"content": text},
                            "finish_reason": null,
                        }]
                    })
                );
                return Some(Bytes::from(sse));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! The SDK-type builders and the stream-event decoder are pure functions
    //! over AWS SDK types, so they are testable without credentials. What is
    //! not: `complete` and `stream` issue signed calls through
    //! `aws_sdk_bedrockruntime::Client`, which this adapter constructs itself
    //! from the ambient AWS credential chain — there is no endpoint seam to
    //! point at a local server, so those two remain live-AWS territory.

    use super::*;
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ConverseStreamOutput, MessageStartEvent,
        ToolUseBlockDelta,
    };
    use serde_json::json;

    #[test]
    fn sdk_messages_drop_system_and_map_roles() {
        let messages = vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
            // An unrecognised role is treated as the caller, not dropped —
            // losing a turn silently would change the conversation.
            json!({"role": "tool", "content": "42"}),
        ];
        let sdk = to_sdk_messages(&messages).unwrap();
        assert_eq!(sdk.len(), 3, "the system turn is not a Converse message");
        assert_eq!(sdk[0].role(), &ConversationRole::User);
        assert_eq!(sdk[1].role(), &ConversationRole::Assistant);
        assert_eq!(sdk[2].role(), &ConversationRole::User);
        assert_eq!(sdk[0].content()[0].as_text().unwrap(), "hello");
    }

    #[test]
    fn sdk_messages_tolerate_a_missing_or_non_string_content() {
        let messages = vec![json!({"role": "user"}), json!({"role": "user", "content": 7})];
        let sdk = to_sdk_messages(&messages).unwrap();
        assert_eq!(sdk.len(), 2);
        assert_eq!(sdk[0].content()[0].as_text().unwrap(), "");
        assert_eq!(sdk[1].content()[0].as_text().unwrap(), "");
    }

    #[test]
    fn sdk_system_keeps_only_system_turns() {
        let messages = vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hello"}),
            json!({"role": "system", "content": "and polite"}),
        ];
        let system = to_sdk_system(&messages);
        assert_eq!(system.len(), 2);
        assert_eq!(system[0].as_text().unwrap(), "be brief");
        assert_eq!(system[1].as_text().unwrap(), "and polite");

        assert!(
            to_sdk_system(&[json!({"role": "user", "content": "hello"})]).is_empty(),
            "no system turns means no system block"
        );
    }

    #[test]
    fn a_text_delta_becomes_one_openai_shaped_sse_frame() {
        let event = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .delta(ContentBlockDelta::Text("wor".to_string()))
                .content_block_index(0)
                .build()
                .unwrap(),
        );
        let bytes = process_stream_event(event).expect("a text delta produces a frame");
        let frame = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(frame.starts_with("data: "), "{frame}");
        assert!(frame.ends_with("\n\n"), "{frame}");
        let payload: serde_json::Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim_end()).unwrap();
        assert_eq!(payload["choices"][0]["delta"]["content"], "wor");
        assert!(payload["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn non_text_events_produce_no_frame() {
        // A tool-use delta carries no assistant text.
        let tool_delta = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .delta(ContentBlockDelta::ToolUse(
                    ToolUseBlockDelta::builder().input("{}").build().unwrap(),
                ))
                .content_block_index(0)
                .build()
                .unwrap(),
        );
        assert!(process_stream_event(tool_delta).is_none());

        // So does a lifecycle event.
        let start = ConverseStreamOutput::MessageStart(
            MessageStartEvent::builder()
                .role(ConversationRole::Assistant)
                .build()
                .unwrap(),
        );
        assert!(process_stream_event(start).is_none());
    }

    #[tokio::test]
    async fn new_honours_a_configured_region() {
        // Region from config short-circuits the region chain, so no metadata
        // lookup happens and construction stays offline.
        let config = ProviderConfig {
            region: Some("us-east-1".to_string()),
            timeout_secs: 5,
            ..Default::default()
        };
        let adapter = BedrockAdapter::new(&config).await;
        assert_eq!(
            adapter.client.config().region().map(|r| r.as_ref()),
            Some("us-east-1")
        );
    }
}
