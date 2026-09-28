use anyhow::Context;
use futures::TryStreamExt;

use crate::config::schema::{ProviderConfig, TierTimeoutsConfig};
use crate::providers::adapter::{
    CompletionResult, EffectiveSettings, NormalizedRequest, ProviderAdapter, SseStream,
};
use crate::providers::azure_credentials::AzureAuth;
use crate::providers::azure_entra::COGNITIVE_SERVICES_SCOPE;

/// GA stable Azure OpenAI API version at time of writing.
/// Operators should pin `api_version` in config for production deployments.
const DEFAULT_API_VERSION: &str = "2024-02-01";

pub struct AzureOpenAIAdapter {
    /// The `api-key` header by default; a Microsoft Entra bearer token when
    /// `credential_source` is set under `[providers.azure]`.
    auth: AzureAuth,
    api_base: String,
    api_version: String,
    client: reqwest::Client,
    default_timeout_secs: u64,
    tier_timeouts: TierTimeoutsConfig,
}

impl AzureOpenAIAdapter {
    /// Infallible constructor kept for existing callers; panics where
    /// [`Self::try_new`] would return an error.
    pub fn new(config: &ProviderConfig, tier_timeouts: TierTimeoutsConfig) -> Self {
        Self::try_new(config, tier_timeouts).unwrap_or_else(|e| panic!("{e:#}"))
    }

    /// Build from config. Key auth (`api-key` header) unless
    /// `credential_source` selects an Entra credential, in which case the
    /// audience is `entra_scope` or the Azure OpenAI default,
    /// `https://cognitiveservices.azure.com/.default`.
    pub fn try_new(
        config: &ProviderConfig,
        tier_timeouts: TierTimeoutsConfig,
    ) -> anyhow::Result<Self> {
        let scope = config
            .entra_scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(COGNITIVE_SERVICES_SCOPE);
        let auth = AzureAuth::key_by_default(
            "azure",
            config,
            scope,
            std::time::Duration::from_secs(config.timeout_secs.max(1)),
        )?;
        Ok(Self::with_auth(config, tier_timeouts, auth))
    }

    /// Build with a caller-chosen auth mode (tests).
    pub fn with_auth(
        config: &ProviderConfig,
        tier_timeouts: TierTimeoutsConfig,
        auth: AzureAuth,
    ) -> Self {
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
            auth,
            api_base,
            api_version,
            client,
            default_timeout_secs: config.timeout_secs,
            tier_timeouts,
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
        // Azure uses the deployment URL for model selection; the `model` field in the body is ignored.
        let mut body = serde_json::json!({
            "messages": req.messages,
            "stream": false,
        });

        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        // Azure serves the OpenAI wire shape: tools pass through verbatim (issue #88).
        if let Some(tools) = &req.tools {
            body["tools"] = serde_json::json!(tools);
        }
        if let Some(tc) = &req.tool_choice {
            body["tool_choice"] = tc.clone();
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
    /// Raw JSON, forwarded verbatim (issue #88).
    tool_calls: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct AzureUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: AzurePromptTokensDetails,
    #[serde(default)]
    completion_tokens_details: AzureCompletionTokensDetails,
}

#[derive(serde::Deserialize, Default)]
struct AzureCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(serde::Deserialize, Default)]
struct AzurePromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[async_trait::async_trait]
impl ProviderAdapter for AzureOpenAIAdapter {
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        self.auth.credential_report()
    }

    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let body = Self::build_body(req);

        let timeout_secs = self.tier_timeouts.resolve(&req.request_model, self.default_timeout_secs);
        let dispatched = std::time::Instant::now();
        let resp = self.client.post(self.chat_url());
        let resp = self
            .auth
            .apply(resp)
            .await?
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Azure OpenAI")?;
        // Headers are in, body not yet read: time to first token.
        let ttft_ms = dispatched.elapsed().as_millis() as i64;

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
            cache_read_tokens: parsed.usage.prompt_tokens_details.cached_tokens,
            cache_write_tokens: 0,
            reasoning_tokens: parsed.usage.completion_tokens_details.reasoning_tokens,
            ttft_ms: Some(ttft_ms),
            tool_calls: choice.message.tool_calls.filter(|tc| !tc.is_null()),
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let mut body = Self::build_body(req);
        body["stream"] = serde_json::json!(true);
        // The router owns usage capture (issue #84): always request the final
        // usage chunk so the streaming ledger records provider-counted tokens.
        body["stream_options"] = serde_json::json!({"include_usage": true});

        let timeout_secs = self
            .tier_timeouts
            .resolve(&req.request_model, self.default_timeout_secs);
        let resp = self.client.post(self.chat_url());
        let resp = self
            .auth
            .apply(resp)
            .await?
            .timeout(std::time::Duration::from_secs(timeout_secs))
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

    /// Azure serves the OpenAI wire shape: `tools` pass through verbatim (issue #88).
    fn supports_tools(&self, _model: &str) -> bool {
        true
    }

    /// Temperature and `max_tokens` are forwarded as normalized; the timeout
    /// is the tier-resolved ceiling this adapter applies to the call.
    fn effective_settings(&self, req: &NormalizedRequest) -> EffectiveSettings {
        EffectiveSettings {
            reasoning: None,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            timeout_secs: Some(
                self.tier_timeouts
                    .resolve(&req.request_model, self.default_timeout_secs),
            ),
        }
    }
}
