//! Claude deployments on Azure AI Foundry, over the Anthropic Messages API.
//!
//! Foundry serves Claude on its own surface, not on the OpenAI-shaped ones the
//! rest of this module speaks:
//!
//! ```text
//! POST https://<resource>.services.ai.azure.com/anthropic/v1/messages
//! Authorization: Bearer <Entra token, scope https://ai.azure.com/.default>
//!   (or: x-api-key: <key>)
//! anthropic-version: 2023-06-01
//! {"model": "<deployment>", "messages": [...], "max_tokens": N, ...}
//! ```
//!
//! The deployment name goes in the body's `model`, as on the other Foundry
//! surfaces. Body building and response/SSE translation are the direct
//! `anthropic` adapter's, so tools, thinking/effort, prompt caching and
//! streaming behave identically. Which deployments take this path is
//! configuration (`anthropic_deployments`), never inferred from the name.

use std::collections::HashSet;

use anyhow::Context;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, SseStream};
use crate::providers::anthropic::{
    apply_prompt_caching, build_body, completion_from_response, translate_sse_stream,
    ANTHROPIC_VERSION,
};
use crate::providers::azure_entra::FOUNDRY_PROJECT_SCOPE;
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::FoundryEndpoint;
use futures::TryStreamExt;

const MESSAGES_PATH: &str = "/anthropic/v1/messages";

pub struct FoundryClaude {
    url: String,
    scope: String,
    auth: FoundryAuth,
    deployments: HashSet<String>,
    prompt_caching: bool,
}

impl FoundryClaude {
    /// `None` when the provider lists no `anthropic_deployments`.
    pub fn from_config(
        config: &ProviderConfig,
        endpoint: &FoundryEndpoint,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<Self>> {
        if config.anthropic_deployments.is_empty() {
            return Ok(None);
        }
        let scope = Self::scope_for(config);
        let auth = FoundryAuth::entra_by_default("foundry", config, &scope, timeout)?;
        Ok(Some(Self::with_auth(config, endpoint, auth)))
    }

    /// Build with a caller-supplied credential (the adapter's test hook).
    pub fn with_auth(config: &ProviderConfig, endpoint: &FoundryEndpoint, auth: FoundryAuth) -> Self {
        let url = Self::messages_url(endpoint.base());
        let scope = Self::scope_for(config);
        tracing::info!(
            url = %url,
            scope = %scope,
            auth = auth.label(),
            deployments = ?config.anthropic_deployments,
            "foundry: Claude deployments use the Anthropic Messages surface"
        );
        Self {
            url,
            scope,
            auth,
            deployments: config.anthropic_deployments.iter().map(|d| d.trim().to_string()).collect(),
            prompt_caching: config.prompt_caching,
        }
    }

    /// `entra_scope` when set, else `ai.azure.com`, the audience Microsoft
    /// documents for the Messages surface on resource and project endpoints.
    fn scope_for(config: &ProviderConfig) -> String {
        config
            .entra_scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(FOUNDRY_PROJECT_SCOPE)
            .to_string()
    }

    /// `{scheme}://{host}/anthropic/v1/messages` for any configured endpoint
    /// shape: a bare resource, a project, `/openai/v1` or `/models`.
    pub fn messages_url(base: &str) -> String {
        let host_start = base.find("://").map(|i| i + 3).unwrap_or(0);
        let origin = match base[host_start..].find('/') {
            Some(i) => &base[..host_start + i],
            None => base,
        };
        format!("{origin}{MESSAGES_PATH}")
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn serves(&self, deployment: &str) -> bool {
        self.deployments.contains(deployment)
    }

    async fn send(
        &self,
        client: &reqwest::Client,
        req: &NormalizedRequest,
        stream: bool,
        timeout: std::time::Duration,
    ) -> anyhow::Result<reqwest::Response> {
        let mut body = build_body(req, stream);
        apply_prompt_caching(&mut body, req, self.prompt_caching);
        let request = client
            .post(&self.url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .timeout(timeout)
            .json(&body);
        let request = match &self.auth {
            FoundryAuth::Entra(provider) => request.bearer_auth(
                provider
                    .token()
                    .await
                    .context("failed to get an Entra token for Azure AI Foundry (Claude)")?,
            ),
            FoundryAuth::ApiKey(key) => request.header("x-api-key", key),
        };
        tracing::debug!(url = %self.url, model = %req.model, stream, auth = self.auth.label(), "foundry: Claude message");
        let resp = request
            .send()
            .await
            .with_context(|| format!("failed to send request to Azure AI Foundry at {}", self.url))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(url = %self.url, %status, model = %req.model, body = text.as_str(), "foundry: Claude message failed");
        Err(self.http_error(&req.model, status, &text))
    }

    pub async fn complete(
        &self,
        client: &reqwest::Client,
        req: &NormalizedRequest,
        timeout: std::time::Duration,
    ) -> anyhow::Result<CompletionResult> {
        let dispatched = std::time::Instant::now();
        let resp = self.send(client, req, false, timeout).await?;
        let ttft_ms = dispatched.elapsed().as_millis() as i64;
        let body: serde_json::Value = resp
            .json()
            .await
            .context("Azure AI Foundry returned a Claude message body that is not JSON")?;
        let mut result = completion_from_response(body)?;
        result.ttft_ms = Some(ttft_ms);
        Ok(result)
    }

    pub async fn stream(
        &self,
        client: &reqwest::Client,
        req: &NormalizedRequest,
        timeout: std::time::Duration,
    ) -> anyhow::Result<SseStream> {
        let resp = self.send(client, req, true, timeout).await?;
        Ok(translate_sse_stream(
            resp.bytes_stream()
                .map_err(|e| anyhow::anyhow!("Azure AI Foundry stream error: {}", e)),
        ))
    }

    fn http_error(&self, model: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
        let hint = match status.as_u16() {
            401 | 403 => format!(
                " — check the identity has the \"Cognitive Services User\" (or equivalent) role \
                 on this resource and that the audience is right: this request used scope {}. \
                 Set `entra_scope` under [providers.foundry] to switch.",
                self.scope
            ),
            404 => format!(
                " — no Claude deployment named \"{model}\" behind {}. Check the name in \
                 `anthropic_deployments` matches the Foundry deployment.",
                self.url
            ),
            _ => String::new(),
        };
        anyhow::anyhow!("Azure AI Foundry returned {status}: {body}{hint}")
    }
}

#[cfg(test)]
mod tests {
    use super::FoundryClaude;

    #[test]
    fn messages_url_is_the_resource_origin_for_every_endpoint_shape() {
        let want = "https://res.services.ai.azure.com/anthropic/v1/messages";
        for base in [
            "https://res.services.ai.azure.com/openai/v1",
            "https://res.services.ai.azure.com/models",
            "https://res.services.ai.azure.com/api/projects/p/openai/v1",
        ] {
            assert_eq!(FoundryClaude::messages_url(base), want, "{base}");
        }
        assert_eq!(
            FoundryClaude::messages_url("http://127.0.0.1:9000/openai/v1"),
            "http://127.0.0.1:9000/anthropic/v1/messages"
        );
    }
}
