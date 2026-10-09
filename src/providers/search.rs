use async_trait::async_trait;

/// Upper bound on `SearchRequest::max_follow_up_queries`; the search route
/// rejects a request above it and adapters cap at it.
pub const MAX_FOLLOW_UP_QUERIES: u32 = 10;

#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: Option<u32>,
    /// Ask a grounding engine to return its generated answer alongside the
    /// citations (`SearchResponse::answer`). Engines with no generated answer
    /// ignore it and return `answer: None`; the results are unchanged either way.
    pub include_answer: bool,
    /// Grounded-research mode (all need `include_answer`): caller instructions
    /// replacing the engine's default answer instruction, extra context
    /// appended after the query, and how many follow-up search queries to ask
    /// for. Engines without a generated answer ignore them.
    pub instructions: Option<String>,
    pub context: Option<String>,
    pub max_follow_up_queries: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub score: Option<f64>,
    pub published_date: Option<String>,
}

/// The generated text a grounding engine wrote around its citations.
///
/// Returned only on request (`SearchRequest::include_answer`). It is model
/// output, not retrieved content: a caller can use it to decide which cited
/// pages to read first or what to search next, but the citations in `results`
/// are the retrieved part of the response.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct SearchAnswer {
    pub text: String,
    /// Search-engine query URLs the provider reported for the tool calls
    /// behind this answer, verbatim. Some providers' display terms require a
    /// caller to show these alongside the results.
    pub query_urls: Vec<String>,
    /// Search queries the model proposed for what the retrieved pages did not
    /// answer; empty unless `max_follow_up_queries` asked for them.
    pub follow_up_queries: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub results: Vec<SearchResultItem>,
    pub engine: String,
    /// Present only when requested and the engine produced one.
    pub answer: Option<SearchAnswer>,
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
