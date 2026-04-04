use modelrouter::db::{migrations::run_migrations, sqlite::SqliteDb};
use modelrouter::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

pub async fn in_memory_db() -> SqliteDb {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    run_migrations(&db.pool).await.unwrap();
    db
}

pub struct MockAdapter {
    pub response: String,
}

#[async_trait::async_trait]
impl ProviderAdapter for MockAdapter {
    async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        Ok(CompletionResult {
            content: self.response.clone(),
            prompt_tokens: 10,
            completion_tokens: 20,
            finish_reason: "stop".to_string(),
        })
    }

    async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        use bytes::Bytes;
        use futures::stream;
        let data = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n",
            self.response
        );
        let stream =
            stream::once(async move { Ok::<Bytes, anyhow::Error>(Bytes::from(data)) });
        Ok(Box::pin(stream))
    }
}

pub struct MockEmbeddingAdapter {
    pub embedding: Vec<f32>,
}

#[async_trait::async_trait]
impl modelrouter::providers::embedding::EmbeddingAdapter for MockEmbeddingAdapter {
    async fn embed(
        &self,
        req: &modelrouter::providers::embedding::EmbeddingRequest,
    ) -> anyhow::Result<modelrouter::providers::embedding::EmbeddingResult> {
        Ok(modelrouter::providers::embedding::EmbeddingResult {
            embeddings: vec![self.embedding.clone(); req.input.len()],
            prompt_tokens: req.input.iter().map(|s| s.len() as u32 / 4).sum(),
        })
    }
}
