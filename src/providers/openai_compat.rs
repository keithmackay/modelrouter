use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

pub struct OpenAICompatAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
}

impl OpenAICompatAdapter {
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
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
    usage: OpenAIUsage,
}

#[derive(serde::Deserialize)]
struct OpenAIChoice {
    message: OpenAIMessage,
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct OpenAIMessage {
    content: Option<String>,
}

#[derive(serde::Deserialize)]
struct OpenAIUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[async_trait::async_trait]
impl ProviderAdapter for OpenAICompatAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let url = format!("{}/chat/completions", self.api_base);

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": false,
        });

        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to OpenAI-compat provider")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Provider returned {}: {}", status, text);
        }

        let parsed: OpenAIResponse = resp
            .json()
            .await
            .context("Failed to parse OpenAI response")?;

        let choice = parsed.choices.into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

        Ok(CompletionResult {
            content: choice.message.content.unwrap_or_default(),
            prompt_tokens: parsed.usage.prompt_tokens,
            completion_tokens: parsed.usage.completion_tokens,
            finish_reason: choice.finish_reason.unwrap_or_else(|| "stop".to_string()),
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let url = format!("{}/chat/completions", self.api_base);

        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": true,
        });

        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send streaming request to OpenAI-compat provider")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Provider returned {}: {}", status, text);
        }

        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("Stream error: {}", e))
            .map_ok(Bytes::from);

        Ok(Box::pin(stream))
    }
}
