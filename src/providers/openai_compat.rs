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
    #[serde(default)]
    prompt_tokens_details: OpenAIPromptTokensDetails,
}

#[derive(serde::Deserialize, Default)]
struct OpenAIPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
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
            cache_read_tokens: parsed.usage.prompt_tokens_details.cached_tokens,
            cache_write_tokens: 0,
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


// ── Catalog discovery (issue #33) ────────────────────────────────────────────

#[async_trait::async_trait]
impl crate::providers::catalog::ProviderCatalog for OpenAICompatAdapter {
    /// GET {api_base}/models — the OpenAI wire shape every compat provider
    /// serves. `provider` is left EMPTY here: this adapter serves many
    /// registry names (openai, groq, ollama, ...), and only the aggregation
    /// caller (#34) knows which key it queried; it rewrites the field.
    async fn list_models(&self) -> anyhow::Result<Vec<crate::providers::catalog::CatalogModel>> {
        let url = format!("{}/models", self.api_base);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("catalog request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("catalog returned {status}: {text}");
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str())
            .map(|id| crate::providers::catalog::CatalogModel {
                provider: String::new(),
                name: id.to_string(),
                display_name: None,
            })
            .collect())
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::providers::catalog::ProviderCatalog;
    use axum::{routing::get, Json, Router};

    fn adapter_for(base: &str) -> OpenAICompatAdapter {
        let mut config = crate::config::schema::ProviderConfig::default();
        config.api_base = Some(base.to_string());
        config.api_key = "k".into();
        OpenAICompatAdapter::new(&config)
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn lists_models_with_empty_provider_for_caller_to_fill() {
        let base = serve(Router::new().route(
            "/models",
            get(|| async {
                Json(serde_json::json!({"data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}]}))
            }),
        ))
        .await;
        let models = adapter_for(&base).list_models().await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["gpt-4o", "gpt-4o-mini"]
        );
        assert!(models.iter().all(|m| m.provider.is_empty()));
    }

    #[tokio::test]
    async fn catalog_error_carries_status() {
        let base = serve(Router::new().route(
            "/models",
            get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "bad key") }),
        ))
        .await;
        let err = adapter_for(&base).list_models().await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
    }
}
