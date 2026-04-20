//! Auth module for the Vertex provider. Exposes a `TokenProvider` trait so
//! the adapter can be tested without touching real Google OAuth.
//!
//! Real auth is wrapped around `google-cloud-auth` 1.9's
//! `AccessTokenCredentials`, which caches and auto-refreshes tokens.

use async_trait::async_trait;

/// Fetches a Google Cloud OAuth2 Bearer access token.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> anyhow::Result<String>;
}

/// Test-only token provider that returns a pre-configured token.
pub struct StaticTokenProvider(String);

impl StaticTokenProvider {
    pub fn new(token: String) -> Self {
        Self(token)
    }
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// Production token provider backed by `google-cloud-auth 1.9`.
///
/// If `credentials_path` is None, uses Application Default Credentials
/// (`gcloud auth application-default login`, `GOOGLE_APPLICATION_CREDENTIALS`
/// env var, or the GCE/GKE/Cloud Run metadata server — whichever is
/// resolvable first).
///
/// If `credentials_path` is Some, the file is read as a service-account JSON
/// and passed to the service-account builder directly.
///
/// Tokens are cached and auto-refreshed by `google-cloud-auth`; `.token()`
/// is safe to call on every request.
pub struct GoogleCloudAuthProvider {
    credentials: google_cloud_auth::credentials::AccessTokenCredentials,
}

impl GoogleCloudAuthProvider {
    pub fn new(credentials_path: Option<&str>) -> anyhow::Result<Self> {
        let credentials = match credentials_path {
            Some(path) => {
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("failed to read {path}: {e}"))?;
                let json: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|e| anyhow::anyhow!("{path} is not valid JSON: {e}"))?;
                google_cloud_auth::credentials::service_account::Builder::new(json)
                    .with_access_specifier(
                        google_cloud_auth::credentials::service_account::AccessSpecifier::from_scopes(
                            [CLOUD_PLATFORM_SCOPE],
                        ),
                    )
                    .build_access_token_credentials()
                    .map_err(|e| anyhow::anyhow!("failed to build service-account credentials: {e}"))?
            }
            None => google_cloud_auth::credentials::Builder::default()
                .with_scopes([CLOUD_PLATFORM_SCOPE])
                .build_access_token_credentials()
                .map_err(|e| anyhow::anyhow!("failed to build ADC credentials: {e}"))?,
        };
        Ok(Self { credentials })
    }
}

#[async_trait]
impl TokenProvider for GoogleCloudAuthProvider {
    async fn token(&self) -> anyhow::Result<String> {
        let access = self
            .credentials
            .access_token()
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch GCP access token: {e}"))?;
        Ok(access.token)
    }
}
