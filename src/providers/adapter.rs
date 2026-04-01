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

#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub finish_reason: String,
}

pub type SseStream = Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Send>>;

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult>;
    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream>;
}
