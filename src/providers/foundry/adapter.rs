//! Chat completions against models deployed in Azure AI Foundry.
//!
//! Distinct from `azure_openai.rs`, which speaks only to Azure OpenAI
//! deployments through a per-deployment `api_base`. A Foundry resource serves a
//! whole catalogue — Llama, Mistral, Phi, DeepSeek, Cohere, Grok and the OpenAI
//! models alike — behind ONE endpoint, with the deployment named in the request
//! body. That is why this adapter takes an endpoint and routes by model
//! (`foundry/<deployment>`), the way the Vertex adapter takes a project and
//! routes by publisher/model.
//!
//! ## Wire shape
//!
//! Both Foundry surfaces are OpenAI-shaped on the wire (see
//! `foundry::endpoint` for which is which and why the OpenAI-compatible one is
//! the default):
//!
//! ```text
//! POST {base}/chat/completions[?api-version=...]
//! Authorization: Bearer <Entra token>   (or: api-key: <key>)
//! {"model": "<deployment>", "messages": [...], "stream": false}
//! ```
//!
//! Responses carry `choices[].message.content`, `choices[].finish_reason` and
//! `usage.{prompt_tokens,completion_tokens}`, plus
//! `usage.prompt_tokens_details.cached_tokens` where the model reports a prompt
//! cache — identical to the OpenAI-compat and Azure OpenAI adapters, which is
//! why the parsing below reads the same.
//!
//! It is NOT implemented by delegating to `OpenAICompatAdapter`: that adapter
//! hardcodes `Authorization: Bearer {api_key}` from config and appends
//! `/chat/completions` to `api_base` with no api-version, so every part of the
//! Azure surface that differs — Entra tokens refreshed per request, the
//! `api-key` header, the required api-version on the Model Inference surface,
//! surface-aware catalog discovery — would have to be bolted on from outside.
//! Standalone, like `azure_openai.rs`.
//!
//! ## No network in this environment
//!
//! Written against the published REST specs; the development host has no route
//! to Azure. Every behaviour below is covered by mocked-HTTP tests
//! (`tests/test_foundry.rs`) and instrumented with tracing at the points where
//! a live deployment would first diverge from the spec: the resolved URL and
//! surface, the credential source and audience, the deployment name, and the
//! status and body of any non-2xx response.

use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};
use crate::providers::azure_entra::TokenProvider;
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::FoundryEndpoint;
use std::sync::Arc;

pub struct FoundryAdapter {
    endpoint: FoundryEndpoint,
    auth: FoundryAuth,
    client: reqwest::Client,
}

impl FoundryAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        let auth = FoundryAuth::from_config(config, endpoint.scope())?;
        Self::build(endpoint, auth, config)
    }

    /// Test hook: build with a caller-supplied token source, bypassing the
    /// Entra round trip. Mirrors `BingGroundingAdapter::with_token_provider`.
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
            .context("failed to build reqwest client for foundry")?;
        tracing::info!(
            endpoint = endpoint.base(),
            surface = endpoint.surface().label(),
            api_version = endpoint.api_version().unwrap_or("(none — server default)"),
            scope = endpoint.scope(),
            auth = auth.label(),
            timeout_secs = config.timeout_secs,
            "foundry: adapter ready"
        );
        Ok(Self {
            endpoint,
            auth,
            client,
        })
    }

    pub fn endpoint(&self) -> &FoundryEndpoint {
        &self.endpoint
    }

    pub(crate) fn auth(&self) -> &FoundryAuth {
        &self.auth
    }

    pub(crate) fn http_client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Body for either surface. `model` carries the DEPLOYMENT name: unlike
    /// Azure OpenAI, where the deployment is in the URL and the body's `model`
    /// is ignored, a Foundry endpoint fronts many deployments and selects on
    /// this field.
    pub fn build_body(req: &NormalizedRequest, stream: bool) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
            "stream": stream,
        });
        // The router owns usage capture (issue #84): always request the final
        // usage chunk so the streaming ledger records provider-counted tokens.
        if stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }
        if let Some(temp) = req.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        body
    }

    /// Turn a non-2xx into an error that says which endpoint, which
    /// deployment, and — for the two statuses an operator actually hits — what
    /// to check. Azure's own body is always included: it carries the AADSTS or
    /// `code` value that names the real fault.
    fn http_error(&self, model: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
        let hint = match status.as_u16() {
            401 | 403 => format!(
                " — check the identity has the \"Azure AI User\" (or equivalent) role on this \
                 resource, and that the audience is right: this request used scope {}. The \
                 published spec and Microsoft's keyless-auth how-to disagree about the audience \
                 for resource endpoints; set `entra_scope` under [providers.foundry] to switch.",
                self.endpoint.scope()
            ),
            404 => format!(
                " — no deployment named \"{model}\" on this endpoint, or the surface is wrong. \
                 This request used the {} surface at {}.",
                self.endpoint.surface().label(),
                self.endpoint.base()
            ),
            _ => String::new(),
        };
        anyhow::anyhow!("Azure AI Foundry returned {status}: {body}{hint}")
    }
}

#[derive(serde::Deserialize)]
struct FoundryResponse {
    choices: Vec<FoundryChoice>,
    #[serde(default)]
    usage: FoundryUsage,
}

#[derive(serde::Deserialize)]
struct FoundryChoice {
    message: FoundryMessage,
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct FoundryMessage {
    content: Option<String>,
}

/// `usage` is required by the spec, but a `default` here keeps a model that
/// omits it from failing the whole call — the completion is still usable, it
/// just meters as zero.
#[derive(serde::Deserialize, Default)]
struct FoundryUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: FoundryPromptTokensDetails,
}

#[derive(serde::Deserialize, Default)]
struct FoundryPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

/// Parse a chat-completions payload into the router's result shape.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let parsed: FoundryResponse = serde_json::from_value(v)
        .context("failed to parse Azure AI Foundry chat completion response")?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in Azure AI Foundry response"))?;
    Ok(CompletionResult {
        content: choice.message.content.unwrap_or_default(),
        prompt_tokens: parsed.usage.prompt_tokens,
        completion_tokens: parsed.usage.completion_tokens,
        finish_reason: choice.finish_reason.unwrap_or_else(|| "stop".to_string()),
        cache_read_tokens: parsed.usage.prompt_tokens_details.cached_tokens,
        cache_write_tokens: 0,
        ttft_ms: None,
    })
}

#[async_trait::async_trait]
impl ProviderAdapter for FoundryAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let url = self.endpoint.chat_url();
        let body = Self::build_body(req, false);
        tracing::debug!(
            url = %url,
            surface = self.endpoint.surface().label(),
            auth = self.auth.label(),
            model = %req.model,
            "foundry: chat completion"
        );

        let request = self
            .auth
            .apply(self.client.post(&url).json(&body))
            .await
            .context("failed to attach credentials to the Azure AI Foundry request")?;

        let dispatched = std::time::Instant::now();
        let resp = request.send().await.with_context(|| {
            format!("failed to send request to Azure AI Foundry at {url}")
        })?;
        // Headers are in, body not yet read: time to first token.
        let ttft_ms = dispatched.elapsed().as_millis() as i64;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                url = %url,
                %status,
                model = %req.model,
                body = text.as_str(),
                "foundry: chat completion failed"
            );
            return Err(self.http_error(&req.model, status, &text));
        }

        let v: serde_json::Value = resp
            .json()
            .await
            .context("Azure AI Foundry returned a chat completion body that is not JSON")?;
        let mut result = parse_response(v)?;
        result.ttft_ms = Some(ttft_ms);
        tracing::debug!(
            model = %req.model,
            prompt_tokens = result.prompt_tokens,
            completion_tokens = result.completion_tokens,
            cached_tokens = result.cache_read_tokens,
            finish_reason = %result.finish_reason,
            ttft_ms,
            "foundry: chat completion ok"
        );
        Ok(result)
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let url = self.endpoint.chat_url();
        let body = Self::build_body(req, true);
        tracing::debug!(
            url = %url,
            surface = self.endpoint.surface().label(),
            auth = self.auth.label(),
            model = %req.model,
            "foundry: streaming chat completion"
        );

        let request = self
            .auth
            .apply(self.client.post(&url).json(&body))
            .await
            .context("failed to attach credentials to the Azure AI Foundry request")?;

        let resp = request.send().await.with_context(|| {
            format!("failed to send streaming request to Azure AI Foundry at {url}")
        })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                url = %url,
                %status,
                model = %req.model,
                body = text.as_str(),
                "foundry: streaming chat completion failed"
            );
            return Err(self.http_error(&req.model, status, &text));
        }

        // Already OpenAI-shaped SSE, terminated by `data: [DONE]`: passed
        // through byte-for-byte, exactly as the OpenAI-compat adapter does.
        // Nothing is re-framed, so a chunk boundary mid-frame stays intact.
        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("Azure AI Foundry stream error: {}", e))
            .map_ok(Bytes::from);

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> NormalizedRequest {
        NormalizedRequest {
            model: "Llama-3.3-70B-Instruct".into(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            stream: false,
            temperature: Some(0.2),
            max_tokens: Some(16),
            extra_params: serde_json::json!({}),
        }
    }

    /// Unlike Azure OpenAI, the deployment is selected by the body's `model`,
    /// not by the URL. Dropping it would send every call to whichever
    /// deployment the endpoint happens to default to.
    #[test]
    fn body_names_the_deployment_and_carries_sampling_params() {
        let body = FoundryAdapter::build_body(&req(), false);
        assert_eq!(body["model"], "Llama-3.3-70B-Instruct");
        assert_eq!(body["stream"], false);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["max_tokens"], 16);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(FoundryAdapter::build_body(&req(), true)["stream"], true);
    }

    #[test]
    fn unset_sampling_params_are_omitted_not_defaulted() {
        let mut r = req();
        r.temperature = None;
        r.max_tokens = None;
        let body = FoundryAdapter::build_body(&r, false);
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn parses_content_tokens_finish_reason_and_prompt_cache() {
        let result = parse_response(serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "length"
            }],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 3,
                "total_tokens": 14,
                "prompt_tokens_details": {"cached_tokens": 8}
            }
        }))
        .unwrap();
        assert_eq!(result.content, "hello");
        assert_eq!(result.prompt_tokens, 11);
        assert_eq!(result.completion_tokens, 3);
        assert_eq!(result.finish_reason, "length");
        assert_eq!(result.cache_read_tokens, 8);
        assert!(result.is_cached());
    }

    /// The spec allows a null `finish_reason` and a null `content`; neither is
    /// a reason to fail a call that otherwise succeeded.
    #[test]
    fn nulls_degrade_to_defaults() {
        let result = parse_response(serde_json::json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": null},
                         "finish_reason": null}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1}
        }))
        .unwrap();
        assert_eq!(result.content, "");
        assert_eq!(result.finish_reason, "stop");
        assert_eq!(result.cache_read_tokens, 0);
    }

    #[test]
    fn an_empty_choices_array_is_an_error_not_an_empty_completion() {
        let err = parse_response(serde_json::json!({"choices": [], "usage": {}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("No choices"), "{err}");
    }
}
