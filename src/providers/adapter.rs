use std::pin::Pin;
use futures::Stream;
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct NormalizedRequest {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub extra_params: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionResult {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub finish_reason: String,
    /// Tokens served from the provider's prompt cache (billed at a reduced rate).
    pub cache_read_tokens: u32,
    /// Tokens written to the provider's prompt cache on this request (billed at a premium rate).
    pub cache_write_tokens: u32,
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
}
