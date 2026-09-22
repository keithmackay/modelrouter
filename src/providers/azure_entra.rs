//! Microsoft Entra ID token acquisition for Azure data planes.
//!
//! Shared by every Azure-facing provider in the tree (`bing_grounding`,
//! `foundry`), which is why it lives here and not inside one of them. The
//! audience differs per endpoint, so the SCOPE is a constructor argument and
//! each caller passes the constant its own API documents — see the two
//! `*_SCOPE` constants below and the per-endpoint rule in
//! `providers/foundry/endpoint.rs`.
//!
//! Hand-rolled on purpose. The only thing a caller needs is one OAuth2 token
//! for one scope, which is a form POST and a GET; pulling in the
//! `azure_identity`/`azure_core` family to get it would add a second async
//! runtime-adjacent dependency tree for ~80 lines of behaviour. `reqwest` and
//! `serde` are already here.
//!
//! Two credential sources, tried in this order:
//!
//! 1. **Client credentials** — `AZURE_TENANT_ID` + `AZURE_CLIENT_ID` +
//!    `AZURE_CLIENT_SECRET` in the environment. This is the app-registration
//!    flow: `POST {authority}/{tenant}/oauth2/v2.0/token`.
//! 2. **Managed identity** — the IMDS endpoint on an Azure VM / App Service /
//!    Container Apps workload. No secret exists at all in this mode.
//!
//! Neither reads a secret from config.toml. That mirrors the Vertex adapter's
//! ADC posture: credentials come from the environment the process runs in, not
//! from a file the router itself ships.
//!
//! Tokens are cached in-process and refreshed shortly before expiry, so
//! `.token()` is safe to call on every request.

use anyhow::Context;
use async_trait::async_trait;
use std::time::{Duration, Instant};

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

/// Public-cloud login authority. Overridable with `AZURE_AUTHORITY_HOST`, the
/// same variable the Azure SDKs honour for sovereign clouds (and the seam the
/// tests point at a local mock, since this host cannot reach Azure).
const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com";

/// Instance Metadata Service. Link-local and unroutable off-host by design.
/// Overridable with `AZURE_POD_IDENTITY_AUTHORITY_HOST`, again matching the
/// Azure SDKs (AKS pod identity) and again the seam the tests use.
const DEFAULT_IMDS_HOST: &str = "http://169.254.169.254";

/// IMDS token API version. Pinned; IMDS requires an explicit one.
const IMDS_API_VERSION: &str = "2018-02-01";

/// Refresh this far ahead of the stated expiry. An hour-long token refreshed
/// five minutes early costs one extra request every twelve hours and removes
/// the class of failure where a token expires in flight.
const EXPIRY_SKEW: Duration = Duration::from_secs(300);

/// Fetches a Bearer token for the Foundry data plane.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> anyhow::Result<String>;
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

/// Which credential flow this process resolved to, with the endpoint already
/// baked in so the network call has nothing left to decide.
#[derive(Debug, Clone)]
pub enum CredentialSource {
    ClientCredentials {
        token_url: String,
        client_id: String,
        client_secret: String,
    },
    ManagedIdentity {
        token_url: String,
        /// Present for a user-assigned identity, absent for system-assigned.
        client_id: Option<String>,
    },
}

impl CredentialSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClientCredentials { .. } => "client_credentials",
            Self::ManagedIdentity { .. } => "managed_identity",
        }
    }

    /// Resolve the credential source from the process environment.
    ///
    /// Deliberately does not fail when the client-credentials trio is
    /// incomplete: a half-configured app registration is far more likely to be
    /// a deployment mistake than a request to use managed identity, so the
    /// partial state is reported as a warning naming the missing variables and
    /// the IMDS path is still attempted.
    ///
    /// `scope` is the audience the caller needs a token for. It shapes only the
    /// managed-identity URL here (IMDS takes the resource as a query
    /// parameter); the client-credentials flow sends it in the form body at
    /// fetch time.
    pub fn from_env(scope: &str) -> Self {
        let tenant = non_empty_env("AZURE_TENANT_ID");
        let client_id = non_empty_env("AZURE_CLIENT_ID");
        let secret = non_empty_env("AZURE_CLIENT_SECRET");

        if let (Some(tenant), Some(client_id), Some(secret)) =
            (tenant.clone(), client_id.clone(), secret.clone())
        {
            let authority = non_empty_env("AZURE_AUTHORITY_HOST")
                .unwrap_or_else(|| DEFAULT_AUTHORITY_HOST.to_string());
            let token_url = format!(
                "{}/{}/oauth2/v2.0/token",
                authority.trim_end_matches('/'),
                tenant
            );
            tracing::info!(
                credential_source = "client_credentials",
                token_url = %token_url,
                client_id = %client_id,
                scope = %scope,
                "azure_entra: using Entra client-credentials from the environment"
            );
            return Self::ClientCredentials {
                token_url,
                client_id,
                client_secret: secret,
            };
        }

        // `AZURE_CLIENT_ID` alone is a legitimate user-assigned managed-identity
        // setup, so it alone is not evidence of a half-finished app
        // registration. A tenant or a secret without the rest is.
        if tenant.is_some() || secret.is_some() {
            let missing: Vec<&str> = [
                ("AZURE_TENANT_ID", tenant.is_none()),
                ("AZURE_CLIENT_ID", client_id.is_none()),
                ("AZURE_CLIENT_SECRET", secret.is_none()),
            ]
            .iter()
            .filter(|(_, absent)| *absent)
            .map(|(name, _)| *name)
            .collect();
            tracing::warn!(
                missing = ?missing,
                "azure_entra: some Entra client-credentials variables are set but not all — \
                 falling back to managed identity. Set the missing variable(s) if an app \
                 registration was intended."
            );
        }

        let imds = non_empty_env("AZURE_POD_IDENTITY_AUTHORITY_HOST")
            .unwrap_or_else(|| DEFAULT_IMDS_HOST.to_string());
        let token_url = format!(
            "{}/metadata/identity/oauth2/token?api-version={}&resource={}",
            imds.trim_end_matches('/'),
            IMDS_API_VERSION,
            resource_for_scope(scope)
        );
        tracing::info!(
            credential_source = "managed_identity",
            token_url = %token_url,
            user_assigned = client_id.is_some(),
            scope = %scope,
            "azure_entra: using managed identity (IMDS) for Entra tokens"
        );
        Self::ManagedIdentity {
            token_url,
            client_id,
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

struct CachedToken {
    token: String,
    /// Already has `EXPIRY_SKEW` subtracted.
    refresh_after: Instant,
}

/// Entra token provider with in-process caching, for one scope.
///
/// One provider serves one audience: the cached token is only valid for the
/// scope it was minted with, so a caller needing a second audience builds a
/// second provider rather than passing a scope per call.
pub struct EntraTokenProvider {
    client: reqwest::Client,
    source: CredentialSource,
    scope: String,
    cache: tokio::sync::RwLock<Option<CachedToken>>,
}

impl EntraTokenProvider {
    /// Resolve credentials from the environment for `scope`.
    /// See [`CredentialSource::from_env`].
    pub fn from_env(scope: &str, timeout: Duration) -> anyhow::Result<Self> {
        Self::with_source(CredentialSource::from_env(scope), scope, timeout)
    }

    pub fn with_source(
        source: CredentialSource,
        scope: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client for Entra token acquisition")?;
        Ok(Self {
            client,
            source,
            scope: scope.to_string(),
            cache: tokio::sync::RwLock::new(None),
        })
    }

    pub fn source_label(&self) -> &'static str {
        self.source.label()
    }

    /// The audience this provider mints tokens for. Logged by callers at
    /// construction: an `AADSTS500011`/401 against a healthy endpoint is almost
    /// always the wrong audience, and the resolved scope is the first thing to
    /// check.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    async fn fetch(&self) -> anyhow::Result<(String, Duration)> {
        let resp = match &self.source {
            CredentialSource::ClientCredentials {
                token_url,
                client_id,
                client_secret,
            } => {
                self.client
                    .post(token_url)
                    .form(&[
                        ("grant_type", "client_credentials"),
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                        ("scope", self.scope.as_str()),
                    ])
                    .send()
                    .await
                    .with_context(|| {
                        format!(
                            "Entra token request to {token_url} failed — check network egress to \
                             the login authority and that AZURE_TENANT_ID names a real tenant"
                        )
                    })?
            }
            CredentialSource::ManagedIdentity {
                token_url,
                client_id,
            } => {
                let mut req = self.client.get(token_url).header("Metadata", "true");
                if let Some(id) = client_id {
                    req = req.query(&[("client_id", id.as_str())]);
                }
                req.send().await.with_context(|| {
                    format!(
                        "managed-identity token request to {token_url} failed — this host has no \
                         reachable IMDS endpoint. Set AZURE_TENANT_ID / AZURE_CLIENT_ID / \
                         AZURE_CLIENT_SECRET to use an app registration instead."
                    )
                })?
            }
        };

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The body of a failed Entra response is the diagnosis (AADSTS
            // codes name the exact misconfiguration), and it contains no
            // secret — the secret travels in the request, not the reply.
            anyhow::bail!(
                "Entra token endpoint returned {} for source {}: {}",
                status,
                self.source.label(),
                body.trim()
            );
        }

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .context("Entra token endpoint returned a body that is not JSON")?;
        let token = parsed["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Entra token response carried no access_token"))?
            .to_string();

        // IMDS reports `expires_in` as a decimal STRING; the OAuth2 endpoint
        // reports it as a number. Accept both rather than silently treating an
        // unparsed value as "expired", which would refetch on every request.
        let expires_in = parsed["expires_in"]
            .as_u64()
            .or_else(|| parsed["expires_in"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(3600);

        Ok((token, Duration::from_secs(expires_in)))
    }
}

#[async_trait]
impl TokenProvider for EntraTokenProvider {
    async fn token(&self) -> anyhow::Result<String> {
        if let Some(cached) = self.cache.read().await.as_ref() {
            if Instant::now() < cached.refresh_after {
                return Ok(cached.token.clone());
            }
        }

        // Write lock held across the fetch so a burst of concurrent requests
        // produces one token request, not one per request.
        let mut slot = self.cache.write().await;
        if let Some(cached) = slot.as_ref() {
            if Instant::now() < cached.refresh_after {
                return Ok(cached.token.clone());
            }
        }

        let (token, lifetime) = self.fetch().await.with_context(|| {
            format!(
                "could not acquire an Entra token via {} for scope {}",
                self.source.label(),
                self.scope
            )
        })?;
        tracing::debug!(
            credential_source = self.source.label(),
            scope = %self.scope,
            lifetime_secs = lifetime.as_secs(),
            "azure_entra: acquired Entra token"
        );
        *slot = Some(CachedToken {
            token: token.clone(),
            refresh_after: Instant::now() + lifetime.saturating_sub(EXPIRY_SKEW),
        });
        Ok(token)
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

    /// The scope reaches the IMDS URL, so two callers with different audiences
    /// really do request different tokens.
    #[test]
    fn managed_identity_url_carries_the_callers_resource() {
        // Sensitive to process env, but only reads it; no mutation, so this
        // needs no serialisation with the env-mutating integration tests.
        let source = CredentialSource::from_env(COGNITIVE_SERVICES_SCOPE);
        if let CredentialSource::ManagedIdentity { token_url, .. } = source {
            assert!(
                token_url.contains("resource=https://cognitiveservices.azure.com"),
                "{token_url}"
            );
        }
    }
}
