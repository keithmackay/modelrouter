use anyhow::Context;
use crate::config::schema::ProviderConfig;
use crate::providers::embedding::{EmbeddingAdapter, EmbeddingRequest, EmbeddingResult};

pub struct OpenAIEmbeddingAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
}

impl OpenAIEmbeddingAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to build reqwest client");
        Self {
            api_key: config.api_key.clone(),
            api_base,
            client,
        }
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    usage: EmbeddingUsage,
}

#[derive(serde::Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[derive(serde::Deserialize)]
struct EmbeddingUsage {
    prompt_tokens: u32,
}

#[async_trait::async_trait]
impl EmbeddingAdapter for OpenAIEmbeddingAdapter {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult> {
        let url = format!("{}/embeddings", self.api_base);

        // OpenAI accepts either a single string or array; always send array
        let mut body = serde_json::json!({
            "model": req.model,
            "input": req.input,
        });
        // OpenAI's v3 embedding models accept a `dimensions` parameter and will
        // emit a shorter vector natively. Passing it through is what makes an
        // openai fallback usable by a caller pinned to another provider's width.
        if let Some(dims) = req.dimensions {
            body["dimensions"] = serde_json::json!(dims);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send embedding request to OpenAI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding provider returned {}: {}", status, text);
        }

        let parsed: EmbeddingResponse = resp
            .json()
            .await
            .context("Failed to parse embedding response")?;

        let result = EmbeddingResult {
            embeddings: parsed.data.into_iter().map(|d| d.embedding).collect(),
            prompt_tokens: parsed.usage.prompt_tokens,
        };
        // Providers that ignore `dimensions` (Ollama and most OpenAI-compatible
        // servers do) would otherwise return their native width and have it
        // accepted silently.
        result.verify_dimensions(req.dimensions)?;
        Ok(result)
    }
}
