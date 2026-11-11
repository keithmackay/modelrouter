//! The decision seam: one request against a model that answers typed
//! questions about a document (predicates as probabilities, choices, scores)
//! instead of generating text.
//!
//! The router does not interpret the request or the answer. The caller's
//! body is passed through with `model` rewritten to the deployment the router
//! resolved, and the provider's body is returned as it arrived, with the
//! router's own `usage` cost fields and `x_router` added. What the router
//! owns is the same as for every other `/v1` call: who may call, which
//! deployment answers, how long it may take, what it cost, and the record of
//! the call.
//!
//! A provider's rejection of the request (a 4xx) is a fact about the request,
//! not about the provider's health: it is returned to the caller with the
//! provider's status and body, and it is never retried.

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct DecisionRequest {
    /// The deployment the router resolved; replaces the caller's `model`.
    pub model: String,
    /// The caller's body, as sent.
    pub body: Value,
    /// Ceiling for the whole call, resolved per request from the router's
    /// tier timeouts (the same rule as chat).
    pub timeout: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct DecisionResult {
    /// The provider's response body, unchanged.
    pub body: Value,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// The provider refused the request itself (an HTTP 4xx). Carried as a typed
/// cause so the route can hand the caller the provider's own status and body
/// instead of a 502, and so nothing on the way back treats it as retryable.
#[derive(Debug, Clone)]
pub struct UpstreamRejected {
    pub status: u16,
    /// The provider's body, parsed when it was JSON, else `{"message": text}`.
    pub body: Value,
}

impl std::fmt::Display for UpstreamRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the provider rejected the request with HTTP {}: {}", self.status, self.body)
    }
}

impl std::error::Error for UpstreamRejected {}

impl UpstreamRejected {
    /// Build from a non-2xx response's status and raw text.
    pub fn from_response(status: u16, text: &str) -> Self {
        let body = serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| serde_json::json!({ "message": text }));
        Self { status, body }
    }

    /// The typed rejection under an `anyhow` chain, if that is what the error is.
    pub fn find(err: &anyhow::Error) -> Option<&UpstreamRejected> {
        err.chain().find_map(|cause| cause.downcast_ref::<UpstreamRejected>())
    }
}

/// The request cannot be sent as it stands: the adapter knows, before any
/// call, that the deployment does not serve decisions. The route answers 400.
#[derive(Debug, Clone)]
pub struct DecisionRequestInvalid(pub String);

impl std::fmt::Display for DecisionRequestInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecisionRequestInvalid {}

impl DecisionRequestInvalid {
    pub fn find(err: &anyhow::Error) -> Option<&DecisionRequestInvalid> {
        err.chain().find_map(|cause| cause.downcast_ref::<DecisionRequestInvalid>())
    }
}

/// Token counts from a provider's `usage` object. Reads the OpenAI names
/// first (`prompt_tokens`/`completion_tokens`) and the Responses-style names
/// (`input_tokens`/`output_tokens`) second, so a provider that uses either
/// is priced; a missing block counts as zero tokens, which the route reports
/// as unpriced rather than free.
pub fn usage_tokens(body: &Value) -> (u32, u32) {
    let usage = &body["usage"];
    let read = |a: &str, b: &str| usage[a].as_u64().or_else(|| usage[b].as_u64()).unwrap_or(0) as u32;
    (read("prompt_tokens", "input_tokens"), read("completion_tokens", "output_tokens"))
}

#[async_trait]
pub trait DecisionAdapter: Send + Sync {
    async fn decide(&self, req: &DecisionRequest) -> anyhow::Result<DecisionResult>;

    /// The credential this adapter authenticates with, for `GET /health/deep`;
    /// `None` for adapters with nothing beyond a static key to report.
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn usage_tokens_read_both_naming_conventions_and_default_to_zero() {
        assert_eq!(usage_tokens(&serde_json::json!({"usage": {"prompt_tokens": 7, "completion_tokens": 2}})), (7, 2));
        assert_eq!(usage_tokens(&serde_json::json!({"usage": {"input_tokens": 5, "output_tokens": 1}})), (5, 1));
        assert_eq!(usage_tokens(&serde_json::json!({"answers": []})), (0, 0));
    }

    #[test]
    fn a_rejection_keeps_the_provider_json_body_or_wraps_plain_text() {
        let json = UpstreamRejected::from_response(400, r#"{"error":{"code":"bad"}}"#);
        assert_eq!(json.body["error"]["code"], "bad");
        let text = UpstreamRejected::from_response(404, "no such deployment");
        assert_eq!(text.body["message"], "no such deployment");
        // A JSON scalar is not a body a caller can read fields from.
        assert_eq!(UpstreamRejected::from_response(400, "42").body["message"], "42");
    }

    #[test]
    fn a_rejection_is_found_under_added_context() {
        let err: anyhow::Result<()> = Err(anyhow::Error::new(UpstreamRejected::from_response(422, "{}")));
        let err = err.context("decision failed").unwrap_err();
        assert_eq!(UpstreamRejected::find(&err).map(|r| r.status), Some(422));
        assert!(DecisionRequestInvalid::find(&err).is_none());
        let plain = anyhow::anyhow!("socket closed");
        assert!(UpstreamRejected::find(&plain).is_none());
    }
}
