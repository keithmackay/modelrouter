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
    /// Restrict results to pages the engine discovered within a window: `day`,
    /// `week`, `month`, a date `YYYY-MM-DD` or a range `YYYY-MM-DD..YYYY-MM-DD`
    /// (validated by `parse_freshness`). Overrides the engine's configured
    /// default. Engines without an age filter ignore it.
    pub freshness: Option<String>,
}

/// Validate a freshness value and return its canonical form: the age words
/// lower-cased, dates as given. `None` when the value is not one of the forms
/// `SearchRequest::freshness` documents, or a range runs backwards.
pub fn parse_freshness(value: &str) -> Option<String> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    if matches!(lower.as_str(), "day" | "week" | "month") {
        return Some(lower);
    }
    let date = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().filter(|_| s.len() == 10);
    match v.split_once("..") {
        Some((from, to)) => match (date(from), date(to)) {
            (Some(a), Some(b)) if a <= b => Some(v.to_string()),
            _ => None,
        },
        None => date(v).map(|_| v.to_string()),
    }
}

/// `SearchResultItem::metadata_provenance` when the publication date or
/// publisher was read out of model-written text rather than reported by the
/// search provider: plausible, but not verified against the page.
pub const PROVENANCE_MODEL_EXTRACTED: &str = "model_extracted";

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub score: Option<f64>,
    pub published_date: Option<String>,
    /// Name of the publishing organisation, when the engine reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Where `published_date` and `publisher` came from when that is not the
    /// provider's own metadata, e.g. `PROVENANCE_MODEL_EXTRACTED`. Absent when
    /// neither field is set or the provider reported them itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_provenance: Option<String>,
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
