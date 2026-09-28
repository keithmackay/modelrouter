//! Microsoft Entra ID token plumbing shared by every Azure-facing provider
//! (`azure`, `bing_grounding`, `foundry`): the audiences each endpoint
//! documents, and the [`TokenProvider`] seam adapters call per request.
//!
//! The audience differs per endpoint, so the SCOPE is chosen by the caller —
//! see the two `*_SCOPE` constants below and the per-endpoint rule in
//! `providers/foundry/endpoint.rs`.
//!
//! The credential sources themselves (managed identity, workload identity,
//! client secret, Azure CLI, and the `default` chain) live in
//! `providers::azure_credentials`, behind the provider-neutral
//! `providers::credentials::CredentialSource` trait, so the shared policy —
//! typed `credential_expired`, the dead-credential cache, fallback probes and
//! the health verdict — is the same one every other provider gets.

use async_trait::async_trait;

/// Scope for a Foundry PROJECT endpoint
/// (`https://<resource>.services.ai.azure.com/api/projects/<project>` — the
/// Agents / Responses surface). Documented as the scope to request in the Bing
/// grounding REST sample,
/// `az account get-access-token --scope "https://ai.azure.com/.default"` —
/// <https://learn.microsoft.com/azure/ai-foundry/agents/how-to/tools/bing-tools>
/// — and repeated for Foundry Models keyless auth in
/// <https://learn.microsoft.com/azure/ai-foundry/model-inference/how-to/configure-entra-id>.
pub const FOUNDRY_PROJECT_SCOPE: &str = "https://ai.azure.com/.default";

/// Scope for the RESOURCE-level inference endpoints — the Azure AI Model
/// Inference surface (`{endpoint}/models`) and the OpenAI-compatible surface
/// (`{endpoint}/openai/v1`, including `*.openai.azure.com`). Both published
/// OpenAPI documents declare exactly this OAuth2 scope:
/// `specification/ai/data-plane/ModelInference/.../openapi.yaml` and
/// `specification/ai/data-plane/OpenAI.v1/azure-v1-v1-generated.yaml` in
/// Azure/azure-rest-api-specs.
pub const COGNITIVE_SERVICES_SCOPE: &str = "https://cognitiveservices.azure.com/.default";

/// Back-compat alias for the scope `bing_grounding` has always requested.
pub const FOUNDRY_SCOPE: &str = FOUNDRY_PROJECT_SCOPE;

/// IMDS wants a RESOURCE (audience) where the OAuth2 token endpoint wants a
/// SCOPE; the two differ only by the `/.default` suffix.
pub fn resource_for_scope(scope: &str) -> &str {
    scope.trim_end_matches("/.default")
}

/// Fetches a Bearer token for an Azure data plane.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> anyhow::Result<String>;

    /// The shared credential report for `/health/deep`. `None` for sources
    /// that are not a real credential (the static test provider).
    fn credential_report(&self) -> Option<crate::providers::credentials::CredentialReport> {
        None
    }
}

/// Test-only provider returning a fixed token.
pub struct StaticTokenProvider(String);

impl StaticTokenProvider {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IMDS is asked for a RESOURCE, the token endpoint for a SCOPE. Getting
    /// this wrong yields a token for the wrong audience, which the data plane
    /// rejects with a 401 that names nothing useful.
    #[test]
    fn resource_strips_the_default_suffix() {
        assert_eq!(resource_for_scope(FOUNDRY_PROJECT_SCOPE), "https://ai.azure.com");
        assert_eq!(
            resource_for_scope(COGNITIVE_SERVICES_SCOPE),
            "https://cognitiveservices.azure.com"
        );
        // Already a resource: left alone rather than mangled.
        assert_eq!(resource_for_scope("https://ai.azure.com"), "https://ai.azure.com");
    }
}
