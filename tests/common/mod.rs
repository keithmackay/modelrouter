pub mod e2e;
pub mod mock_llm;

use modelrouter::api::app::DatabaseProvider;
use modelrouter::api::auth::hash_token;
use modelrouter::db::models::{CostLedgerEntry, NewApiKey, NewUser};
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::db::repositories::costs::CostRepository;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::{migrations::run_migrations, sqlite::SqliteDb};
use modelrouter::providers::adapter::{
    CompletionResult, NormalizedRequest, ProviderAdapter, SseStream,
};

// Each test crate compiles this module on its own, so an item one crate does
// not use is dead code there; the allows keep the shared helpers warning-free.

pub async fn in_memory_db() -> SqliteDb {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    run_migrations(&db.pool).await.unwrap();
    db
}

/// A cutoff after every row, so `list_cost_entries_before(FOREVER)` is "all".
#[allow(dead_code)]
pub const FOREVER: &str = "2999-01-01T00:00:00Z";

/// Create a user with one API key whose bearer token is `token`; returns the
/// user id.
#[allow(dead_code)]
pub async fn create_user(db: &impl DatabaseProvider, name: &str, token: &str) -> i64 {
    UserRepository::create(
        db,
        NewUser {
            name: name.to_string(),
            email: None,
        },
    )
    .await
    .unwrap();
    let user = UserRepository::find_by_name(db, name).await.unwrap().unwrap();
    ApiKeyRepository::create_api_key(
        db,
        NewApiKey {
            user_id: user.id,
            key_hash: hash_token(token),
            label: Some(name.to_string()),
            expires_at: None,
            project: None,
            session_window_secs: None,
        },
    )
    .await
    .unwrap();
    user.id
}

/// Cost logging is fire-and-forget, so poll until at least `want` ledger rows
/// exist rather than sleeping for a guessed interval.
#[allow(dead_code)]
pub async fn wait_for_ledger_rows(db: &dyn DatabaseProvider, want: usize) -> Vec<CostLedgerEntry> {
    for _ in 0..200 {
        let rows = CostRepository::list_cost_entries_before(db, FOREVER)
            .await
            .unwrap();
        if rows.len() >= want {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {want} cost-ledger rows");
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
            ..Default::default()
        })
    }

    async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        use bytes::Bytes;
        use futures::stream;
        let data = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n",
            self.response
        );
        let stream = stream::once(async move { Ok::<Bytes, anyhow::Error>(Bytes::from(data)) });
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

pub struct MockSearchAdapter {
    pub results: Vec<modelrouter::providers::search::SearchResultItem>,
}

#[async_trait::async_trait]
impl modelrouter::providers::search::SearchAdapter for MockSearchAdapter {
    async fn search(
        &self,
        _req: &modelrouter::providers::search::SearchRequest,
    ) -> anyhow::Result<modelrouter::providers::search::SearchResponse> {
        Ok(modelrouter::providers::search::SearchResponse {
            results: self.results.clone(),
            engine: "tavily".to_string(),
        })
    }
}
