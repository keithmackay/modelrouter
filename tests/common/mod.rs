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
