//! Microsoft Entra ID token acquisition for the Azure AI Foundry data plane.
//!
//! Hand-rolled on purpose. The only thing this provider needs is one OAuth2
//! token for one scope, which is a form POST and a GET; pulling in the
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

/// Scope for the Foundry data plane (Agents / Responses API). Documented as the
/// scope to request in the Bing grounding REST sample:
/// `az account get-access-token --scope "https://ai.azure.com/.default"` —
/// <https://learn.microsoft.com/azure/ai-foundry/agents/how-to/tools/bing-tools>
pub const FOUNDRY_SCOPE: &str = "https://ai.azure.com/.default";

/// Resource form of the same audience, which IMDS wants instead of a scope.
const FOUNDRY_RESOURCE: &str = "https://ai.azure.com";

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
    pub fn from_env() -> Self {
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
                "bing_grounding: using Entra client-credentials from the environment"
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
                "bing_grounding: some Entra client-credentials variables are set but not all — \
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
            FOUNDRY_RESOURCE
        );
        tracing::info!(
            credential_source = "managed_identity",
            token_url = %token_url,
            user_assigned = client_id.is_some(),
            "bing_grounding: using managed identity (IMDS) for Entra tokens"
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

/// Entra token provider with in-process caching.
pub struct EntraTokenProvider {
    client: reqwest::Client,
    source: CredentialSource,
    cache: tokio::sync::RwLock<Option<CachedToken>>,
}

impl EntraTokenProvider {
    /// Resolve credentials from the environment. See [`CredentialSource::from_env`].
    pub fn from_env(timeout: Duration) -> anyhow::Result<Self> {
        Self::with_source(CredentialSource::from_env(), timeout)
    }

    pub fn with_source(source: CredentialSource, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build HTTP client for Entra token acquisition")?;
        Ok(Self {
            client,
            source,
            cache: tokio::sync::RwLock::new(None),
        })
    }

    pub fn source_label(&self) -> &'static str {
        self.source.label()
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
                        ("scope", FOUNDRY_SCOPE),
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
                FOUNDRY_SCOPE
            )
        })?;
        tracing::debug!(
            credential_source = self.source.label(),
            lifetime_secs = lifetime.as_secs(),
            "bing_grounding: acquired Entra token"
        );
        *slot = Some(CachedToken {
            token: token.clone(),
            refresh_after: Instant::now() + lifetime.saturating_sub(EXPIRY_SKEW),
        });
        Ok(token)
    }
}
