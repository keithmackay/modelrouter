//! VertexAdapter — implements `ProviderAdapter` by routing requests to the
//! Gemini or Claude translator based on the model identifier, then posting
//! to Vertex with a Google Cloud OAuth2 Bearer token.

use std::sync::Arc;
use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};
use crate::providers::vertex::auth::{GoogleCloudAuthProvider, TokenProvider};
use crate::providers::vertex::dispatch::{parse_model_id, Publisher};
use crate::providers::vertex::{claude, gemini};

/// Build the full Vertex REST URL for a given (project, region, publisher, model).
/// For Gemini streaming, appends `?alt=sse` so the server emits line-framed SSE.
pub fn build_endpoint_url(
    project: &str,
    region: &str,
    publisher: Publisher,
    model: &str,
    streaming: bool,
) -> String {
    let (pub_segment, method) = match (publisher, streaming) {
        (Publisher::Google, false) => ("google", "generateContent"),
        (Publisher::Google, true) => ("google", "streamGenerateContent"),
        (Publisher::Anthropic, false) => ("anthropic", "rawPredict"),
        (Publisher::Anthropic, true) => ("anthropic", "streamRawPredict"),
    };
    let mut url = format!(
        "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/{pub_segment}/models/{model}:{method}"
    );
    if matches!(publisher, Publisher::Google) && streaming {
        url.push_str("?alt=sse");
    }
    url
}

pub struct VertexAdapter {
    project: String,
    region: String,
    token_provider: Arc<dyn TokenProvider>,
    client: reqwest::Client,
}

impl VertexAdapter {
    /// Build a VertexAdapter from config, using real Google OAuth.
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let project = config
            .project
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex provider requires `project` in config"))?;
        let region = config
            .region
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex provider requires `region` in config"))?;
        let token_provider = Arc::new(
            GoogleCloudAuthProvider::new(config.credentials_path.as_deref())?,
        ) as Arc<dyn TokenProvider>;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            project,
            region,
            token_provider,
            client,
        })
    }

    /// Test hook: build a VertexAdapter with a caller-supplied token provider
    /// (e.g. `StaticTokenProvider`), bypassing Google OAuth.
    pub fn with_token_provider(
        project: String,
        region: String,
        token_provider: Arc<dyn TokenProvider>,
        timeout_secs: u64,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self {
            project,
            region,
            token_provider,
            client,
        })
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for VertexAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let (publisher, model) = parse_model_id(&req.model)?;
        let url = build_endpoint_url(&self.project, &self.region, publisher, &model, false);
        let body = match publisher {
            Publisher::Google => gemini::translate_request(req),
            Publisher::Anthropic => claude::translate_request(req),
        };
        let token = self.token_provider.token().await?;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("failed to send request to Vertex AI")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Vertex AI returned {}: {}", status, text);
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse Vertex response")?;
        match publisher {
            Publisher::Google => gemini::parse_response(v),
            Publisher::Anthropic => claude::parse_response(v),
        }
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let (publisher, model) = parse_model_id(&req.model)?;
        let url = build_endpoint_url(&self.project, &self.region, publisher, &model, true);
        let body = match publisher {
            Publisher::Google => gemini::translate_request(req),
            Publisher::Anthropic => claude::translate_request(req),
        };
        let token = self.token_provider.token().await?;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("failed to send streaming request to Vertex AI")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Vertex AI streaming returned {}: {}", status, text);
        }

        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("stream error: {}", e))
            .map_ok(move |chunk| {
                let text = String::from_utf8_lossy(&chunk);
                let mut out = String::new();
                for line in text.lines() {
                    let translated = match publisher {
                        Publisher::Google => gemini::translate_sse_line(line),
                        Publisher::Anthropic => claude::translate_sse_line(line),
                    };
                    if let Some(b) = translated {
                        out.push_str(&String::from_utf8_lossy(&b));
                    }
                }
                Bytes::from(out)
            });
        Ok(Box::pin(stream))
    }
}
