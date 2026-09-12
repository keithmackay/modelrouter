//! Embeddings against models deployed in Azure AI Foundry.
//!
//! ```text
//! POST {base}/embeddings[?api-version=...]
//! {"model": "<deployment>", "input": ["..."], "dimensions": 768}
//! ```
//!
//! Response shape is OpenAI's on both surfaces —
//! `data[].embedding`, `usage.prompt_tokens` — per
//! `EmbeddingsResult`/`EmbeddingItem` in the Model Inference OpenAPI and the
//! OpenAI-compatible document.
//!
//! `dimensions` is passed through where the caller pins one: the spec has the
//! field ("returns a 422 if the model doesn't support the value"), and a model
//! that ignores it would otherwise hand back its native width. The result is
//! verified against the request afterwards for exactly that reason — see
//! `EmbeddingResult::verify_dimensions`.
//!
//! No batch splitting here, unlike Vertex: the Foundry embeddings surface takes
//! the whole `input` array in one call and documents no five-instance cap, so
//! splitting would add round trips without cause.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::azure_entra::TokenProvider;
use crate::providers::embedding::{EmbeddingAdapter, EmbeddingRequest, EmbeddingResult};
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::FoundryEndpoint;

pub struct FoundryEmbeddingAdapter {
    endpoint: FoundryEndpoint,
    auth: FoundryAuth,
    client: reqwest::Client,
}

impl FoundryEmbeddingAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        let auth = FoundryAuth::from_config(config, endpoint.scope())?;
        Self::build(endpoint, auth, config)
    }

    /// Test hook: caller-supplied token source, no Entra round trip.
    pub fn with_token_provider(
        config: &ProviderConfig,
        token_provider: Arc<dyn TokenProvider>,
    ) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        Self::build(endpoint, FoundryAuth::Entra(token_provider), config)
    }

    fn build(
        endpoint: FoundryEndpoint,
        auth: FoundryAuth,
        config: &ProviderConfig,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("failed to build reqwest client for foundry embeddings")?;
        tracing::info!(
            endpoint = endpoint.base(),
            surface = endpoint.surface().label(),
            scope = endpoint.scope(),
            auth = auth.label(),
            "foundry: embedding adapter ready"
        );
        Ok(Self {
            endpoint,
            auth,
            client,
        })
    }
}

pub fn build_request_body(req: &EmbeddingRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": req.model,
        "input": req.input,
    });
    if let Some(dims) = req.dimensions {
        body["dimensions"] = serde_json::json!(dims);
    }
    body
}

pub fn parse_response(v: serde_json::Value) -> anyhow::Result<EmbeddingResult> {
    let data = v["data"]
        .as_array()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| anyhow::anyhow!("No embeddings returned from Azure AI Foundry"))?;

    let mut embeddings = Vec::with_capacity(data.len());
    for item in data {
        // The spec allows `embedding` to be a base64 string instead of an
        // array when `encoding_format` asks for it. This adapter never asks,
        // so a string here means the deployment ignored the default — refuse
        // rather than return an empty vector that would be stored as one.
        let values = item["embedding"].as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "Azure AI Foundry returned an embedding that is not an array of numbers — the \
                 deployment may be answering in base64 `encoding_format`, which this adapter \
                 does not request"
            )
        })?;
        embeddings.push(
            values
                .iter()
                .map(|n| n.as_f64().unwrap_or(0.0) as f32)
                .collect::<Vec<f32>>(),
        );
    }

    Ok(EmbeddingResult {
        embeddings,
        prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
    })
}

#[async_trait::async_trait]
impl EmbeddingAdapter for FoundryEmbeddingAdapter {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult> {
        let url = self.endpoint.embeddings_url();
        let body = build_request_body(req);
        tracing::debug!(
            url = %url,
            surface = self.endpoint.surface().label(),
            auth = self.auth.label(),
            model = %req.model,
            inputs = req.input.len(),
            dimensions = req.dimensions,
            "foundry: embedding request"
        );

        let request = self
            .auth
            .apply(self.client.post(&url).json(&body))
            .await
            .context("failed to attach credentials to the Azure AI Foundry embedding request")?;

        let resp = request.send().await.with_context(|| {
            format!("failed to send embedding request to Azure AI Foundry at {url}")
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                url = %url,
                %status,
                model = %req.model,
                body = text.as_str(),
                "foundry: embedding request failed"
            );
            anyhow::bail!("Azure AI Foundry embeddings returned {status}: {text}");
        }

        let result = parse_response(
            resp.json()
                .await
                .context("Azure AI Foundry returned an embedding body that is not JSON")?,
        )?;
        // A wrong-width vector is worse than a failed call: it is stored, and
        // then silently corrupts every similarity comparison made against it.
        result.verify_dimensions(req.dimensions)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> EmbeddingRequest {
        EmbeddingRequest {
            model: "text-embedding-3-large".into(),
            input: vec!["a".into(), "b".into()],
            dimensions: Some(3),
        }
    }

    #[test]
    fn body_sends_the_whole_input_array_and_the_pinned_width() {
        let body = build_request_body(&req());
        assert_eq!(body["model"], "text-embedding-3-large");
        assert_eq!(body["input"], serde_json::json!(["a", "b"]));
        assert_eq!(body["dimensions"], 3);
    }

    #[test]
    fn dimensions_are_omitted_when_the_caller_pins_none() {
        let mut r = req();
        r.dimensions = None;
        assert!(build_request_body(&r).get("dimensions").is_none());
    }

    #[test]
    fn parses_vectors_in_order_with_prompt_tokens() {
        let result = parse_response(serde_json::json!({
            "object": "list",
            "data": [
                {"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]},
                {"object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6]}
            ],
            "usage": {"prompt_tokens": 7, "total_tokens": 7}
        }))
        .unwrap();
        assert_eq!(result.embeddings.len(), 2);
        assert_eq!(result.embeddings[0].len(), 3);
        assert!((result.embeddings[1][0] - 0.4).abs() < 1e-6);
        assert_eq!(result.prompt_tokens, 7);
    }

    #[test]
    fn an_empty_data_array_is_an_error() {
        let err = parse_response(serde_json::json!({"data": [], "usage": {}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No embeddings"), "{err}");
    }

    #[test]
    fn a_base64_embedding_is_refused_rather_than_zeroed() {
        let err = parse_response(serde_json::json!({
            "data": [{"index": 0, "embedding": "eyJ2ZWN0b3IiOjF9"}],
            "usage": {"prompt_tokens": 1}
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("base64"), "{err}");
    }
}
