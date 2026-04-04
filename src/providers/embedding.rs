use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingResult {
    pub embeddings: Vec<Vec<f32>>,
    pub prompt_tokens: u32,
}

#[async_trait]
pub trait EmbeddingAdapter: Send + Sync {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult>;
}
