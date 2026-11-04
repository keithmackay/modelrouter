//! The Decisions API against a deployment in Azure AI Foundry.
//!
//! ```text
//! POST {base}/decisions[?api-version=...]
//! Authorization: Bearer <Entra token>   (or: api-key: <key>)
//! {"model": "<deployment>", "input": "<document>",
//!  "questions": [{"type": "predicate", "name": "...", "instructions": "..."}]}
//! ```
//!
//! The response carries `answers[]` in question order (a predicate answers
//! with a `probability`, the other question types with `choice` or `score`)
//! and a `usage` block. The request is passed through as the caller sent it,
//! with `model` set to the resolved deployment, and the response is returned
//! as it arrived: this adapter reads only `usage`, to price the call. The
//! wire shape is as documented at the time of writing and is not re-shaped
//! here, so a field the service adds or renames reaches the caller untouched.
//!
//! Status handling: a 4xx is the service refusing THIS request (an unknown
//! deployment, a deployment without the decision capability, a malformed
//! question) and is returned to the caller as [`UpstreamRejected`], never
//! retried. A 5xx or a transport failure is a provider error.

use anyhow::Context;
use std::sync::Arc;

use crate::config::schema::ProviderConfig;
use crate::providers::azure_entra::TokenProvider;
use crate::providers::decision::{
    usage_tokens, DecisionAdapter, DecisionRequest, DecisionRequestInvalid, DecisionResult, UpstreamRejected,
};
use crate::providers::foundry::auth::FoundryAuth;
use crate::providers::foundry::endpoint::FoundryEndpoint;

pub struct FoundryDecisionAdapter {
    endpoint: FoundryEndpoint,
    auth: FoundryAuth,
    client: reqwest::Client,
    /// `decision_deployments`: when non-empty, the only deployments a decision
    /// request may name; anything else is refused before a call is made.
    decision_deployments: Vec<String>,
}

impl FoundryDecisionAdapter {
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
    pub fn with_token_provider(
        config: &ProviderConfig,
        token_provider: Arc<dyn TokenProvider>,
    ) -> anyhow::Result<Self> {
        let endpoint = FoundryEndpoint::from_config(config)?;
        Self::build(endpoint, FoundryAuth::Entra(token_provider), config)
    }

    fn build(endpoint: FoundryEndpoint, auth: FoundryAuth, config: &ProviderConfig) -> anyhow::Result<Self> {
        // The client carries the provider's flat ceiling; each request
        // applies its own (tier-resolved) timeout on top, like chat.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .context("failed to build reqwest client for foundry decisions")?;
        let decision_deployments = config
            .decision_deployments
            .iter()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect::<Vec<_>>();
        tracing::info!(
            endpoint = endpoint.base(),
            surface = endpoint.surface().label(),
            scope = endpoint.scope(),
            auth = auth.label(),
            decision_deployments = ?decision_deployments,
            "foundry: decision adapter ready"
        );
        Ok(Self { endpoint, auth, client, decision_deployments })
    }

    /// Refuse, before any call, a deployment outside `decision_deployments`.
    fn check_deployment(&self, model: &str) -> anyhow::Result<()> {
        if self.decision_deployments.is_empty() || self.decision_deployments.iter().any(|d| d == model) {
            return Ok(());
        }
        Err(anyhow::Error::new(DecisionRequestInvalid(format!(
            "deployment \"{model}\" is not one of this endpoint's decision deployments \
             ({}); set [providers.foundry] decision_deployments to add it",
            self.decision_deployments.join(", ")
        ))))
    }
}

/// The body on the wire: the caller's body with `model` set to the resolved
/// deployment. Nothing else is read, added or removed.
pub fn build_request_body(req: &DecisionRequest) -> serde_json::Value {
    let mut body = match &req.body {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => serde_json::json!({}),
    };
    body["model"] = serde_json::Value::String(req.model.clone());
    body
}

#[async_trait::async_trait]
impl DecisionAdapter for FoundryDecisionAdapter {
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        self.auth.credential_report()
    }

    async fn decide(&self, req: &DecisionRequest) -> anyhow::Result<DecisionResult> {
        self.check_deployment(&req.model)?;
        let url = self.endpoint.decisions_url();
        let body = build_request_body(req);
        tracing::debug!(
            url = %url,
            surface = self.endpoint.surface().label(),
            auth = self.auth.label(),
            model = %req.model,
            questions = body["questions"].as_array().map(|q| q.len()).unwrap_or(0),
            timeout_secs = req.timeout.as_secs(),
            "foundry: decision request"
        );

        let request = self
            .auth
            .apply(self.client.post(&url).timeout(req.timeout).json(&body))
            .await
            .context("failed to attach credentials to the Azure AI Foundry decision request")?;
        let resp = request
            .send()
            .await
            .with_context(|| format!("failed to send decision request to Azure AI Foundry at {url}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!(
                url = %url,
                %status,
                model = %req.model,
                body = text.as_str(),
                "foundry: decision request failed"
            );
            if status.is_client_error() {
                // The service refused this request: the caller's to read and act
                // on, with the service's own body. Not retried, no fallback.
                return Err(anyhow::Error::new(UpstreamRejected::from_response(status.as_u16(), &text)));
            }
            anyhow::bail!(
                "Azure AI Foundry decisions returned {status} for deployment \"{}\" at {url}: {text}",
                req.model
            );
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Azure AI Foundry returned a decision body that is not JSON")?;
        let (prompt_tokens, completion_tokens) = usage_tokens(&body);
        tracing::debug!(
            model = %req.model,
            answers = body["answers"].as_array().map(|a| a.len()).unwrap_or(0),
            prompt_tokens,
            completion_tokens,
            "foundry: decision ok"
        );
        Ok(DecisionResult { body, prompt_tokens, completion_tokens })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str) -> DecisionRequest {
        DecisionRequest {
            model: model.into(),
            body: serde_json::json!({
                "model": "alias-the-caller-used",
                "input": "doc",
                "questions": [{"type": "predicate", "name": "q1", "instructions": "Is it?"}],
                "extra": {"kept": true}
            }),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    #[test]
    fn the_body_is_the_callers_with_the_resolved_deployment_as_model() {
        let body = build_request_body(&req("dep-1"));
        assert_eq!(body["model"], "dep-1");
        assert_eq!(body["input"], "doc");
        assert_eq!(body["questions"][0]["name"], "q1");
        // Unknown fields travel untouched: the router does not re-shape the request.
        assert_eq!(body["extra"]["kept"], true);
    }

    #[test]
    fn a_non_object_body_still_names_the_deployment() {
        let mut r = req("dep-1");
        r.body = serde_json::Value::Null;
        assert_eq!(build_request_body(&r), serde_json::json!({"model": "dep-1"}));
    }

    #[test]
    fn a_deployment_outside_the_configured_list_is_refused_by_name() {
        let config = ProviderConfig {
            foundry_endpoint: Some("https://res.services.ai.azure.com".into()),
            decision_deployments: vec!["dep-a".into(), " dep-b ".into(), "".into()],
            ..Default::default()
        };
        let adapter = FoundryDecisionAdapter::with_token_provider(
            &config,
            Arc::new(crate::providers::azure_entra::StaticTokenProvider::new("t")),
        )
        .unwrap();
        adapter.check_deployment("dep-a").unwrap();
        adapter.check_deployment("dep-b").unwrap();
        let err = adapter.check_deployment("dep-c").unwrap_err();
        let invalid = DecisionRequestInvalid::find(&err).expect("typed refusal");
        assert!(invalid.0.contains("\"dep-c\""), "{}", invalid.0);
        assert!(invalid.0.contains("dep-a, dep-b"), "{}", invalid.0);
    }

    #[test]
    fn an_empty_list_passes_every_deployment_through() {
        let config = ProviderConfig {
            foundry_endpoint: Some("https://res.services.ai.azure.com".into()),
            ..Default::default()
        };
        let adapter = FoundryDecisionAdapter::with_token_provider(
            &config,
            Arc::new(crate::providers::azure_entra::StaticTokenProvider::new("t")),
        )
        .unwrap();
        adapter.check_deployment("anything").unwrap();
    }
}
