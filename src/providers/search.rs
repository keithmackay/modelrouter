use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub score: Option<f64>,
    pub published_date: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub results: Vec<SearchResultItem>,
    pub engine: String,
}

#[async_trait]
pub trait SearchAdapter: Send + Sync {
    async fn search(&self, req: &SearchRequest) -> anyhow::Result<SearchResponse>;

    /// The credential this adapter authenticates with, for `GET /health/deep`
    /// — its type, source and the router's verdict, never the credential
    /// itself. `None` (the default) for adapters with nothing beyond a static
    /// key to report.
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        None
    }
}
