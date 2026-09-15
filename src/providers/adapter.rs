use std::pin::Pin;
use futures::Stream;
use bytes::Bytes;

#[derive(Debug, Clone, Default)]
pub struct NormalizedRequest {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    /// OpenAI-shaped function tools (`[{"type":"function","function":{...}}]`),
    /// forwarded only to adapters that declare tool support (issue #88).
    /// `None` when the caller sent no tools or an empty array.
    pub tools: Option<Vec<serde_json::Value>>,
    /// OpenAI-shaped `tool_choice` (`"auto"`, `"required"`, `"none"`, or a
    /// named-function object). `None` when absent or null.
    pub tool_choice: Option<serde_json::Value>,
    pub extra_params: serde_json::Value,
}

/// Serializable so the response cache can persist it in any store backend.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CompletionResult {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub finish_reason: String,
    /// Tokens served from the provider's prompt cache (billed at a reduced rate).
    pub cache_read_tokens: u32,
    /// Tokens written to the provider's prompt cache on this request (billed at a premium rate).
    pub cache_write_tokens: u32,
    /// Time to first token: elapsed time of the provider HTTP send (headers
    /// received, body not yet read). `None` where the client gives no
    /// header/body split (e.g. AWS SDK) — and meaningless on a result replayed
    /// from the response cache, so cache-hit metering must not persist it.
    pub ttft_ms: Option<i64>,
    /// OpenAI-shaped `tool_calls` array when the model asked to invoke tools
    /// (issue #88). Adapters for non-OpenAI wire formats translate their
    /// native shape (e.g. Anthropic `tool_use` blocks) into this one.
    /// `#[serde(default)]` on the struct keeps cached pre-tools payloads
    /// deserializable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<serde_json::Value>,
}

impl CompletionResult {
    /// A prompt is considered "cached" if any tokens were served from cache.
    pub fn is_cached(&self) -> bool {
        self.cache_read_tokens > 0
    }
}

pub type SseStream = Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Send>>;

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult>;
    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream>;

    /// Whether this adapter forwards `tools`/`tool_choice` to `model` (issue
    /// #88). Defaults to `false`: an adapter that has not implemented tool
    /// forwarding must cause a clear 400 at the gate rather than silently
    /// dropping the caller's tools — a tool-less dispatch of an agentic
    /// request produces a plausible-looking but useless answer.
    fn supports_tools(&self, _model: &str) -> bool {
        false
    }
}
