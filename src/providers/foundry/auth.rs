//! How a `foundry` request authenticates.
//!
//! Two modes, exactly as `bing_grounding` has them, because they are the two
//! the endpoints accept: both published OpenAPI documents list an `api-key`
//! header scheme alongside the OAuth2/Bearer scheme.
//!
//! Entra is the intended mode and the default. Key auth is opt-in and nothing
//! but opt-in: it engages only when `api_key` is non-empty in the provider
//! table, which means an operator wrote a secret to disk deliberately.

use std::sync::Arc;
use std::time::Duration;

use crate::config::schema::ProviderConfig;
use crate::providers::azure_entra::{EntraTokenProvider, TokenProvider};

pub enum FoundryAuth {
    /// Intended mode: no secret on disk. Managed identity, or an app
    /// registration in the environment. See `providers::azure_entra`.
    Entra(Arc<dyn TokenProvider>),
    /// Explicit opt-in, selected by setting a non-empty `api_key`.
    ApiKey(String),
}

impl FoundryAuth {
    /// Build from config for the given audience. `scope` comes from the
    /// resolved endpoint (see `foundry::endpoint`), never from a constant here
    /// — a project endpoint and a resource endpoint take different audiences.
    pub fn from_config(config: &ProviderConfig, scope: &str) -> anyhow::Result<Self> {
        if !config.api_key.trim().is_empty() {
            tracing::info!(
                "foundry: `api_key` is set, so key auth (the `api-key` header) is used instead \
                 of Entra. Entra — managed identity, or an app registration in the environment \
                 — is the intended mode; a key in config.toml is a secret at rest."
            );
            return Ok(Self::ApiKey(config.api_key.trim().to_string()));
        }

        let provider = EntraTokenProvider::from_env(
            scope,
            Duration::from_secs(config.timeout_secs.max(1)),
        )?;
        tracing::info!(
            credential_source = provider.source_label(),
            scope = provider.scope(),
            "foundry: authenticating with Entra"
        );
        Ok(Self::Entra(Arc::new(provider) as Arc<dyn TokenProvider>))
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Entra(_) => "entra",
            Self::ApiKey(_) => "api_key",
        }
    }

    /// Attach credentials to an outgoing request. Async because the Entra mode
    /// may have to mint a token (cached in-process; usually a no-op).
    pub async fn apply(
        &self,
        req: reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(match self {
            Self::Entra(provider) => req.bearer_auth(provider.token().await?),
            Self::ApiKey(key) => req.header("api-key", key),
        })
    }
}
