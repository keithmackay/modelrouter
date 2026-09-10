use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::{ProviderConfig, TierTimeoutsConfig};
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

pub struct OpenAICompatAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
    default_timeout_secs: u64,
    tier_timeouts: TierTimeoutsConfig,
}

impl OpenAICompatAdapter {
    pub fn new(config: &ProviderConfig, tier_timeouts: TierTimeoutsConfig) -> Self {
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
            default_timeout_secs: config.timeout_secs,
            tier_timeouts,
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

        let timeout_secs = self.tier_timeouts.resolve(&req.request_model, self.default_timeout_secs);
        let dispatched = std::time::Instant::now();
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .json(&body)
            .send()
            .await
            .context("Failed to send request to OpenAI-compat provider")?;
        // Headers are in, body not yet read: time to first token.
        let ttft_ms = dispatched.elapsed().as_millis() as i64;

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
            ttft_ms: Some(ttft_ms),
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

        let timeout_secs = self.tier_timeouts.resolve(&req.request_model, self.default_timeout_secs);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(timeout_secs))
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
        OpenAICompatAdapter::new(&config, crate::config::schema::TierTimeoutsConfig::default())
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

#[cfg(test)]
mod tier_timeout_tests {
    use super::*;
    use crate::config::schema::{ProviderConfig, TierTimeoutsConfig};
    use axum::{routing::post, Json, Router};

    async fn serve_with_delay(delay: std::time::Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/chat/completions",
            post(move || async move {
                tokio::time::sleep(delay).await;
                Json(serde_json::json!({
                    "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn req(request_model: &str) -> NormalizedRequest {
        NormalizedRequest {
            model: "gpt-4o".into(),
            request_model: request_model.into(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            stream: false,
            temperature: None,
            max_tokens: None,
            extra_params: serde_json::Value::Null,
        }
    }

    /// The core claim of #2020's fix: a request addressing a KNOWN tier gets
    /// THAT tier's ceiling, not the provider's flat `timeout_secs` — proven
    /// here by making the tier ceiling shorter than the provider default and
    /// shorter than the server's response delay, so only the tier value
    /// could be the one that fires.
    #[tokio::test]
    async fn a_known_tier_alias_is_bounded_by_its_own_ceiling_not_the_provider_default() {
        let base = serve_with_delay(std::time::Duration::from_millis(300)).await;
        let config = ProviderConfig {
            api_base: Some(base),
            api_key: "k".into(),
            // Provider default is generous — if this were the value applied,
            // the call would succeed. Only the tier's own tiny ceiling explains a failure.
            timeout_secs: 30,
            ..Default::default()
        };
        let tier_timeouts = TierTimeoutsConfig { fast: 0, balanced: 600, deep: 1800 };
        let adapter = OpenAICompatAdapter::new(&config, tier_timeouts);

        let err = adapter.complete(&req("fast")).await.unwrap_err();
        assert!(
            format!("{err:#}").to_lowercase().contains("time"),
            "expected a timeout-shaped error, got: {err}"
        );
    }

    /// The mirror case: an address that ISN'T a known tier (a literal
    /// `provider/model` string, or an untiered alias) must fall back to the
    /// provider's own flat `timeout_secs`, ignoring `[tier_timeouts]`
    /// entirely — proven by giving every tier a GENEROUS ceiling (so the
    /// call would succeed if any of them applied) while the provider's own
    /// timeout is too short for the server's delay.
    #[tokio::test]
    async fn an_unrecognized_request_model_uses_the_providers_own_timeout_not_any_tier() {
        let base = serve_with_delay(std::time::Duration::from_millis(300)).await;
        let config = ProviderConfig {
            api_base: Some(base),
            api_key: "k".into(),
            timeout_secs: 0,
            ..Default::default()
        };
        let tier_timeouts = TierTimeoutsConfig { fast: 120, balanced: 600, deep: 1800 };
        let adapter = OpenAICompatAdapter::new(&config, tier_timeouts);

        let err = adapter
            .complete(&req("anthropic/claude-opus-4-5"))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").to_lowercase().contains("time"),
            "expected a timeout-shaped error (proving no tier ceiling was mistakenly applied), got: {err}"
        );
    }
}
