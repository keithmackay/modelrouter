//! The rerank seam: one request that orders candidate documents by their
//! relevance to a query, answered by a reranker deployment instead of a chat
//! model.
//!
//! The router does not interpret the request or the answer. The caller's
//! body is passed through with `model` rewritten to the deployment the router
//! resolved, and the provider's body is returned as it arrived, with the
//! router's own `usage` cost fields and `x_router` added. What the router
//! owns is the same as for every other `/v1` call: who may call, which
//! deployment answers, how long it may take, what it cost, and the record of
//! the call. A reranker is billed per search unit (one query against a batch
//! of documents), not per token, so the adapter reads the provider's billed
//! units and the route prices them at a flat rate.
//!
//! A provider's rejection of the request (a 4xx) is a fact about the request,
//! not about the provider's health: it is returned to the caller with the
//! provider's status and body, and it is never retried.

use async_trait::async_trait;
use serde_json::Value;

pub use crate::providers::decision::UpstreamRejected;

#[derive(Debug, Clone)]
pub struct RerankRequest {
    /// The deployment the router resolved; replaces the caller's `model`.
    pub model: String,
    /// The caller's body, as sent.
    pub body: Value,
    /// Ceiling for the whole call, resolved per request from the router's
    /// tier timeouts (the same rule as chat).
    pub timeout: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct RerankResult {
    /// The provider's response body, unchanged.
    pub body: Value,
    /// Billed search units, as the provider reported them (zero when it did not).
    pub search_units: u32,
    /// Input tokens, when the provider reports them beside the units (zero when it does not).
    pub input_tokens: u32,
}

/// The request cannot be sent as it stands: the adapter knows, before any
/// call, that the deployment does not serve reranking. The route answers 400.
#[derive(Debug, Clone)]
pub struct RerankRequestInvalid(pub String);

impl std::fmt::Display for RerankRequestInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RerankRequestInvalid {}

impl RerankRequestInvalid {
    pub fn find(err: &anyhow::Error) -> Option<&RerankRequestInvalid> {
        err.chain().find_map(|cause| cause.downcast_ref::<RerankRequestInvalid>())
    }
}

fn first_u64(body: &Value, paths: &[&[&str]]) -> u32 {
    paths
        .iter()
        .find_map(|path| path.iter().try_fold(body, |v, key| v.get(*key)).and_then(Value::as_u64))
        .unwrap_or(0) as u32
}

/// Billed search units from a rerank body: the Cohere v2 shape
/// (`meta.billed_units.search_units`) first, then a `usage.search_units` a
/// provider may use instead. Zero when neither is present: the route then
/// records the call at $0 and reports it unpriced, never as free.
pub fn billed_search_units(body: &Value) -> u32 {
    first_u64(body, &[&["meta", "billed_units", "search_units"], &["usage", "search_units"]])
}

/// Input tokens, when the provider reports them (`meta.tokens.input_tokens`
/// on Cohere v2, else the OpenAI names); zero otherwise.
pub fn input_tokens(body: &Value) -> u32 {
    first_u64(body, &[&["meta", "tokens", "input_tokens"], &["usage", "prompt_tokens"], &["usage", "input_tokens"]])
}

/// How many results the provider returned (`results[]`), for the record.
pub fn result_count(body: &Value) -> usize {
    body["results"].as_array().map(|r| r.len()).unwrap_or(0)
}

#[async_trait]
pub trait RerankAdapter: Send + Sync {
    async fn rerank(&self, req: &RerankRequest) -> anyhow::Result<RerankResult>;

    /// The credential this adapter authenticates with, for `GET /health/deep`;
    /// `None` for adapters with nothing beyond a static key to report.
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_units_read_the_cohere_shape_then_a_usage_block_and_default_to_zero() {
        assert_eq!(billed_search_units(&serde_json::json!({"meta": {"billed_units": {"search_units": 2}}})), 2);
        assert_eq!(billed_search_units(&serde_json::json!({"usage": {"search_units": 3}})), 3);
        assert_eq!(billed_search_units(&serde_json::json!({"results": []})), 0);
    }

    #[test]
    fn input_tokens_read_both_shapes_and_results_are_counted() {
        assert_eq!(input_tokens(&serde_json::json!({"meta": {"tokens": {"input_tokens": 41}}})), 41);
        assert_eq!(input_tokens(&serde_json::json!({"usage": {"prompt_tokens": 7}})), 7);
        assert_eq!(input_tokens(&serde_json::json!({})), 0);
        assert_eq!(result_count(&serde_json::json!({"results": [{"index": 1}, {"index": 0}]})), 2);
        assert_eq!(result_count(&serde_json::json!({"id": "x"})), 0);
    }

    #[test]
    fn an_invalid_request_is_found_under_added_context() {
        use anyhow::Context;
        let err: anyhow::Result<()> = Err(anyhow::Error::new(RerankRequestInvalid("not a reranker".into())));
        let err = err.context("rerank failed").unwrap_err();
        assert_eq!(RerankRequestInvalid::find(&err).map(|e| e.0.as_str()), Some("not a reranker"));
        assert!(UpstreamRejected::find(&err).is_none());
    }
}
