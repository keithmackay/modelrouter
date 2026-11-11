//! Reranking against a deployment in Azure AI Foundry.
//!
//! ```text
//! POST {resource root}/providers/cohere/v2/rerank      (or [providers.foundry] rerank_url)
//! Authorization: Bearer <Entra token>   (or: api-key: <key>)
//! {"model": "<deployment>", "query": "...", "documents": ["...", ...], "top_n": N}
//! ```
//!
//! The rerankers Foundry serves today speak the Cohere rerank API, which the
//! Azure OpenAI and Model Inference surfaces do not carry, so the request goes
//! to the resource root's Cohere provider path by default; `rerank_url` points
//! the adapter anywhere else (a serverless endpoint's own `/v2/rerank`). The
//! response carries `results[]` (each an `index` into the documents and a
//! `relevance_score`) and `meta.billed_units.search_units`. The request is
//! passed through as the caller sent it, with `model` set to the resolved
//! deployment, and the response is returned as it arrived: this adapter reads
//! only the billed units and tokens, to price the call. A field the service
//! adds or renames reaches the caller untouched.
//!
//! Status handling: a 4xx is the service refusing THIS request (an unknown
//! deployment, a deployment that is not a reranker, a malformed body) and is
//! returned to the caller as [`UpstreamRejected`], never retried. A 5xx or a
//! transport failure is a provider error.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::azure_entra::TokenProvider;
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::FoundryEndpoint;
use crate::providers::rerank::{
    billed_search_units, input_tokens, result_count, RerankAdapter, RerankRequest, RerankRequestInvalid, RerankResult,
    UpstreamRejected,
};

pub struct FoundryRerankAdapter {
    url: String,
    surface: &'static str,
    auth: FoundryAuth,
    client: reqwest::Client,
    /// `rerank_deployments`: when non-empty, the only deployments a rerank
    /// request may name; anything else is refused before a call is made.
    rerank_deployments: Vec<String>,
}

impl FoundryRerankAdapter {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        let auth = FoundryAuth::entra_by_default(
            "foundry",
            config,
            endpoint.scope(),
            std::time::Duration::from_secs(config.timeout_secs.max(1)),
        )?;
        Self::build(endpoint, auth, config)
    }

    /// Test hook: caller-supplied token source, no Entra round trip.
    pub fn with_token_provider(config: &ProviderConfig, token_provider: Arc<dyn TokenProvider>) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        Self::build(endpoint, FoundryAuth::Entra(token_provider), config)
    }

    fn build(endpoint: FoundryEndpoint, auth: FoundryAuth, config: &ProviderConfig) -> anyhow::Result<Self> {
        // The client carries the provider's flat ceiling; each request
        // applies its own (tier-resolved) timeout on top, like chat.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .context("failed to build reqwest client for foundry rerank")?;
        let rerank_deployments = config
            .rerank_deployments
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect::<Vec<_>>();
        let url = endpoint.rerank_url(config.rerank_url.as_deref());
        tracing::info!(
            url = url.as_str(),
            surface = endpoint.surface().label(),
            scope = endpoint.scope(),
            auth = auth.label(),
            rerank_deployments = ?rerank_deployments,
            "foundry: rerank adapter ready"
        );
        Ok(Self { url, surface: endpoint.surface().label(), auth, client, rerank_deployments })
    }

    /// Where requests go, for the record and the tests.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Refuse, before any call, a deployment outside `rerank_deployments`.
    fn check_deployment(&self, model: &str) -> anyhow::Result<()> {
        if self.rerank_deployments.is_empty() || self.rerank_deployments.iter().any(|d| d == model) {
            return Ok(());
        }
        Err(anyhow::Error::new(RerankRequestInvalid(format!(
            "deployment \"{model}\" is not one of this endpoint's rerank deployments \
             ({}); set [providers.foundry] rerank_deployments to add it",
            self.rerank_deployments.join(", ")
        ))))
    }
}

/// The body on the wire: the caller's body with `model` set to the resolved
/// deployment. Nothing else is read, added or removed.
pub fn build_request_body(req: &RerankRequest) -> serde_json::Value {
    let mut body = match &req.body {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => serde_json::json!({}),
    };
    body["model"] = serde_json::Value::String(req.model.clone());
    body
}

#[async_trait::async_trait]
impl RerankAdapter for FoundryRerankAdapter {
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        self.auth.credential_report()
    }

    async fn rerank(&self, req: &RerankRequest) -> anyhow::Result<RerankResult> {
        self.check_deployment(&req.model)?;
        let url = &self.url;
        let body = build_request_body(req);
        tracing::debug!(
            url = %url,
            surface = self.surface,
            auth = self.auth.label(),
            model = %req.model,
            documents = body["documents"].as_array().map(|d| d.len()).unwrap_or(0),
            timeout_secs = req.timeout.as_secs(),
            "foundry: rerank request"
        );

        let request = self
            .auth
            .apply(self.client.post(url).timeout(req.timeout).json(&body))
            .await
            .context("failed to attach credentials to the Azure AI Foundry rerank request")?;
        let resp = request
            .send()
            .await
            .with_context(|| format!("failed to send rerank request to Azure AI Foundry at {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(url = %url, %status, model = %req.model, body = text.as_str(), "foundry: rerank request failed");
            if status.is_client_error() {
                // The service refused this request: the caller's to read and act
                // on, with the service's own body. Not retried, no fallback.
                return Err(anyhow::Error::new(UpstreamRejected::from_response(status.as_u16(), &text)));
            }
            anyhow::bail!("Azure AI Foundry rerank returned {status} for deployment \"{}\" at {url}: {text}", req.model);
        }

        let body: serde_json::Value = resp.json().await.context("Azure AI Foundry returned a rerank body that is not JSON")?;
        let (search_units, input_tokens) = (billed_search_units(&body), input_tokens(&body));
        tracing::debug!(model = %req.model, results = result_count(&body), search_units, input_tokens, "foundry: rerank ok");
        Ok(RerankResult { body, search_units, input_tokens })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::azure_entra::StaticTokenProvider;

    fn req(model: &str) -> RerankRequest {
        RerankRequest {
            model: model.into(),
            body: serde_json::json!({
                "model": "alias-the-caller-used",
                "query": "onboarding time",
                "documents": ["a", "b"],
                "top_n": 1,
                "extra": {"kept": true}
            }),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    fn config(deployments: Vec<String>, url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            foundry_endpoint: Some("https://res.services.ai.azure.com".into()),
            rerank_deployments: deployments,
            rerank_url: url.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn the_body_is_the_callers_with_the_resolved_deployment_as_model() {
        let body = build_request_body(&req("rerank-1"));
        assert_eq!(body["model"], "rerank-1");
        assert_eq!(body["query"], "onboarding time");
        assert_eq!(body["documents"].as_array().unwrap().len(), 2);
        assert_eq!(body["extra"]["kept"], true);
    }

    #[test]
    fn the_default_url_is_the_resource_roots_cohere_rerank_path_and_rerank_url_overrides_it() {
        let adapter = FoundryRerankAdapter::with_token_provider(&config(vec![], None), Arc::new(StaticTokenProvider::new("t"))).unwrap();
        assert_eq!(adapter.url(), "https://res.services.ai.azure.com/providers/cohere/v2/rerank");
        let own = config(vec![], Some("https://rerank-a.westeurope.models.ai.azure.com/v2/rerank/"));
        let adapter = FoundryRerankAdapter::with_token_provider(&own, Arc::new(StaticTokenProvider::new("t"))).unwrap();
        assert_eq!(adapter.url(), "https://rerank-a.westeurope.models.ai.azure.com/v2/rerank");
    }

    #[tokio::test]
    async fn a_deployment_outside_the_configured_list_is_refused_by_name() {
        let adapter = FoundryRerankAdapter::with_token_provider(
            &config(vec!["rerank-a".into(), " rerank-b ".into(), "".into()], None),
            Arc::new(StaticTokenProvider::new("t")),
        )
        .unwrap();
        let err = adapter.rerank(&req("rerank-c")).await.err().expect("refused");
        let invalid = RerankRequestInvalid::find(&err).expect("typed refusal");
        assert!(invalid.0.contains("\"rerank-c\"") && invalid.0.contains("rerank-a, rerank-b"), "{}", invalid.0);
        assert!(adapter.check_deployment("rerank-b").is_ok());
    }
}
