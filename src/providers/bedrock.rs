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
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

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
        let aws_config = loader.load().await;
        let client = aws_sdk_bedrockruntime::Client::new(&aws_config);
        Self { client }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for BedrockAdapter {
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

        loop {
            match event_stream.recv().await {
                Ok(Some(event)) => {
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
                                chunks.push(Ok(Bytes::from(sse)));
                            }
                        }
                    }
                    // Non-text events (metadata, message start/stop) are silently skipped.
                }
                Ok(None) => break,
                Err(e) => {
                    chunks.push(Err(anyhow::anyhow!("Bedrock stream error: {}", e)));
                    break;
                }
            }
        }

        chunks.push(Ok(Bytes::from("data: [DONE]\n\n")));
        Ok(Box::pin(stream::iter(chunks)))
    }
}
