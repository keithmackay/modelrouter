use anyhow::Context;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

const DEFAULT_API_VERSION: &str = "2024-02-01";

pub struct AzureOpenAIAdapter {
    api_key: String,
    api_base: String,
    api_version: String,
    client: reqwest::Client,
}

impl AzureOpenAIAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let api_base = config.api_base.clone().unwrap_or_else(|| {
            panic!(
                "Azure OpenAI adapter requires `api_base` to be set. \
                 Configure it as the full deployment endpoint, e.g.: \
                 https://{{resource}}.openai.azure.com/openai/deployments/{{deployment-name}}"
            )
        });
        let api_version = config
            .api_version
            .clone()
            .unwrap_or_else(|| DEFAULT_API_VERSION.to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to build reqwest client");
        Self {
            api_key: config.api_key.clone(),
            api_base,
            api_version,
            client,
        }
    }

    /// Returns the full chat completions URL including api-version query param.
    pub fn chat_url(&self) -> String {
        format!(
            "{}/chat/completions?api-version={}",
            self.api_base, self.api_version
        )
    }

    fn build_body(req: &NormalizedRequest) -> serde_json::Value {
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

        body
    }
}

#[derive(serde::Deserialize)]
struct AzureResponse {
    choices: Vec<AzureChoice>,
    usage: AzureUsage,
}

#[derive(serde::Deserialize)]
struct AzureChoice {
    message: AzureMessage,
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct AzureMessage {
    content: Option<String>,
}

#[derive(serde::Deserialize)]
struct AzureUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[async_trait::async_trait]
impl ProviderAdapter for AzureOpenAIAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let body = Self::build_body(req);

        let resp = self
            .client
            .post(self.chat_url())
            .header("api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Azure OpenAI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Azure OpenAI returned {}: {}", status, text);
        }

        let parsed: AzureResponse = resp
            .json()
            .await
            .context("Failed to parse Azure OpenAI response")?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No choices in Azure response"))?;

        Ok(CompletionResult {
            content: choice.message.content.unwrap_or_default(),
            prompt_tokens: parsed.usage.prompt_tokens,
            completion_tokens: parsed.usage.completion_tokens,
            finish_reason: choice.finish_reason.unwrap_or_else(|| "stop".to_string()),
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let mut body = Self::build_body(req);
        body["stream"] = serde_json::json!(true);

        let resp = self
            .client
            .post(self.chat_url())
            .header("api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .context("Failed to send streaming request to Azure OpenAI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Azure OpenAI streaming returned {}: {}", status, text);
        }

        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("Stream error: {}", e));

        Ok(Box::pin(stream))
    }
}
