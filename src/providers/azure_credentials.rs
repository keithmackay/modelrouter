//! Microsoft Entra ID credentials as [`CredentialSource`]s, for every
//! Azure-facing provider (`azure`, `foundry`, `bing_grounding`).
//!
//! IMPLEMENTED FROM MICROSOFT'S PUBLISHED DOCUMENTATION; not yet exercised
//! against a live Azure endpoint. The fake-server tests below pin the request
//! shapes the documents describe. The live-test checklist in the README says
//! how to prove each source once an Azure environment is available.
//!
//! Hand-rolled over the `reqwest` client already in the tree rather than
//! pulling in `azure_identity`/`azure_core`: each source is one documented
//! HTTP request (or one CLI invocation), and the shared failure policy lives in
//! `providers::credentials` rather than in an SDK.
//!
//! Sources, selected by `credential_source` (see `AzureCredentialSource`):
//!
//! | value | how | documented at |
//! |---|---|---|
//! | `managed-identity` | `GET $IDENTITY_ENDPOINT?api-version=2019-08-01&resource=..` with `X-IDENTITY-HEADER: $IDENTITY_HEADER` (App Service, Container Apps, Functions); otherwise `GET http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource=..` with `Metadata: true` (VMs, AKS node identity) | learn.microsoft.com: "How to use managed identities for App Service and Azure Functions" (REST endpoint reference); "How to use managed identities for Azure resources on an Azure VM to acquire an access token" |
//! | `workload-identity` | `POST {authority}/{tenant}/oauth2/v2.0/token` with `grant_type=client_credentials`, `client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer` and the federated token file as `client_assertion` | "Microsoft identity platform application authentication certificate credentials" / "Workload identity federation" |
//! | `client-secret` | the same endpoint with `client_secret` | "Microsoft identity platform and the OAuth 2.0 client credentials flow" |
//! | `cli` | `az account get-access-token --resource <resource> --output json` | "az account get-access-token" reference |
//! | `default` | the first of: client secret (all three variables present), workload identity (token file present), managed identity (`IDENTITY_ENDPOINT` set, or IMDS answering a short probe), Azure CLI | mirrors the Azure SDKs' `DefaultAzureCredential` order |
//!
//! When `credential_source` is unset, the Entra-by-default providers
//! (`foundry`, `bing_grounding`) keep their earlier behaviour, reported as the
//! source `environment`: the client secret from the environment when all three
//! variables are set, otherwise managed identity.
//!
//! Reauth classification (permanent, becomes `credential_expired`): the AADSTS
//! codes in [`REAUTH_AADSTS_CODES`], the OAuth errors `interaction_required`
//! and `invalid_grant`, and Azure CLI output that asks for `az login` or
//! carries an AADSTS code. Network failures, 5xx and 429 stay transient.

use crate::config::schema::{AzureCredentialSource, ProviderConfig};
use crate::providers::azure_entra::{resource_for_scope, TokenProvider};
use crate::providers::credentials::{
    CredentialChain, CredentialReport, CredentialSource, CredentialStatus, CredentialVerdict,
    FallbackFactory,
};
use anyhow::Context;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Public-cloud login authority. Overridable with `AZURE_AUTHORITY_HOST`, the
/// variable the Azure SDKs honour for sovereign clouds.
pub const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com";

/// Instance Metadata Service. Overridable with
/// `AZURE_POD_IDENTITY_AUTHORITY_HOST`, matching the Azure SDKs.
pub const DEFAULT_IMDS_HOST: &str = "http://169.254.169.254";

/// Pinned API versions: both endpoints require an explicit one.
const IMDS_API_VERSION: &str = "2018-02-01";
const APP_SERVICE_API_VERSION: &str = "2019-08-01";

/// Refresh this far ahead of the stated expiry (halved for tokens shorter than
/// twice this), so a token never expires in flight.
const EXPIRY_SKEW: Duration = Duration::from_secs(300);

/// Assumed lifetime when a response states none. Deliberately short: an
/// early refresh costs one request, a late one fails a call.
const UNSTATED_LIFETIME: Duration = Duration::from_secs(300);

/// How long `default` waits for IMDS while resolving. Off Azure the
/// link-local address either refuses at once or is answered by another
/// platform's metadata server with a non-Azure reply; this bounds a
/// black-holed route.
const IMDS_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on one `az` invocation.
const CLI_TIMEOUT: Duration = Duration::from_secs(60);

/// AADSTS codes that mean the grant, session or assertion is dead until a
/// human acts:
/// - 50173: the grant expired (password changed or tokens revoked)
/// - 70043: refresh token expired under a sign-in frequency policy
/// - 700082: refresh token expired due to inactivity
/// - 50076, 50079, 50078: multi-factor authentication required / expired
/// - 700024: the client assertion (federated token) is outside its validity
/// - 7000222: the client secret has expired
/// - 7000215: the client secret is invalid
pub const REAUTH_AADSTS_CODES: &[&str] =
    &["50173", "70043", "700082", "50076", "50079", "50078", "700024", "7000222", "7000215"];

/// OAuth error codes that are permanent whatever AADSTS code accompanies them.
const REAUTH_OAUTH_ERRORS: &[&str] = &["interaction_required", "invalid_grant"];

// ── errors ──────────────────────────────────────────────────────────────────

/// A token endpoint answered with a non-success status.
#[derive(Debug, thiserror::Error)]
#[error("{source_kind} token endpoint returned {status}: {body}")]
pub struct AzureTokenError {
    pub source_kind: &'static str,
    pub status: u16,
    /// The endpoint's error body: AADSTS codes and descriptions, never token
    /// material (the secret travels in the request, not the reply).
    pub body: String,
}

/// `az account get-access-token` exited non-zero.
#[derive(Debug, thiserror::Error)]
#[error("Azure CLI exited with {code}: {stderr}")]
pub struct AzureCliError {
    pub code: String,
    pub stderr: String,
}

fn aadsts_codes(text: &str) -> Vec<String> {
    let upper = text.to_ascii_uppercase();
    let mut codes = Vec::new();
    let mut rest = upper.as_str();
    while let Some(at) = rest.find("AADSTS") {
        let digits: String = rest[at + 6..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            codes.push(digits);
        }
        rest = &rest[at + 6..];
    }
    codes
}

fn text_is_reauth(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    REAUTH_OAUTH_ERRORS.iter().any(|e| lower.contains(e))
        || aadsts_codes(text).iter().any(|c| REAUTH_AADSTS_CODES.contains(&c.as_str()))
}

/// Whether an error from any Azure source means the credential is dead.
pub fn is_azure_reauth(err: &anyhow::Error) -> bool {
    if let Some(http) = err.chain().find_map(|e| e.downcast_ref::<AzureTokenError>()) {
        if http.status >= 500 || http.status == 429 {
            return false;
        }
        return text_is_reauth(&http.body);
    }
    if let Some(cli) = err.chain().find_map(|e| e.downcast_ref::<AzureCliError>()) {
        let lower = cli.stderr.to_ascii_lowercase();
        return lower.contains("az login") || lower.contains("aadsts") || text_is_reauth(&cli.stderr);
    }
    text_is_reauth(&format!("{err:#}"))
}

// ── kinds and verdicts ──────────────────────────────────────────────────────

/// The concrete Azure credential types. `default` reports whichever of these
/// it resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AzureKind {
    ManagedIdentity,
    WorkloadIdentity,
    ClientSecret,
    Cli,
}

impl AzureKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::ManagedIdentity => "managed-identity",
            Self::WorkloadIdentity => "workload-identity",
            Self::ClientSecret => "client-secret",
            Self::Cli => "cli",
        }
    }

    /// `azure-managed-identity` etc.
    pub fn label(self) -> String {
        format!("azure-{}", self.suffix())
    }

    /// The router's judgment of this credential type while it works.
    pub fn verdict(self, provider: &str) -> CredentialVerdict {
        match self {
            Self::ManagedIdentity => CredentialVerdict::ok("the platform's managed identity"),
            Self::WorkloadIdentity => CredentialVerdict::ok("workload identity federation"),
            Self::ClientSecret => CredentialVerdict::new(
                CredentialStatus::Warn,
                "app registration client secret; expires on a fixed date and is a secret at rest",
                format!(
                    "track the secret's expiry, or set credential_source = \"managed-identity\" \
                     (or \"workload-identity\") under [providers.{provider}]"
                ),
            ),
            Self::Cli => CredentialVerdict::new(
                CredentialStatus::Warn,
                "personal Azure CLI login; expires under the tenant's sign-in policy",
                format!(
                    "set credential_source = \"managed-identity\" (or \"workload-identity\") under \
                     [providers.{provider}] to use the host's workload identity"
                ),
            ),
        }
    }

    fn expired_guidance(self, provider: &str) -> (String, String) {
        match self {
            Self::ManagedIdentity => (
                "the managed identity was refused".into(),
                format!(
                    "Check that the identity is assigned to this host and holds a role on the \
                     resource [providers.{provider}] calls; for a user-assigned identity check \
                     azure_client_id."
                ),
            ),
            Self::WorkloadIdentity => (
                "the federated token was refused".into(),
                "Check the federated credential on the app registration (issuer, subject, \
                 audience) and that the token file is being refreshed."
                    .into(),
            ),
            Self::ClientSecret => (
                "the client secret was refused (expired, rotated or invalid)".into(),
                "Create a new client secret on the app registration and update \
                 AZURE_CLIENT_SECRET (or azure_client_secret)."
                    .into(),
            ),
            Self::Cli => (
                "the Azure CLI login can no longer be refreshed without interactive sign-in".into(),
                format!(
                    "Run az login, or set credential_source = \"managed-identity\" under \
                     [providers.{provider}] to use the host's workload identity."
                ),
            ),
        }
    }
}

// ── token parsing and caching ───────────────────────────────────────────────

fn number_or_string(v: &serde_json::Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Extract the token and its remaining lifetime from any of the documented
/// response shapes: OAuth2 (`expires_in` number), IMDS (`expires_in` string),
/// App Service (`expires_on` unix-seconds string), Azure CLI (`accessToken`
/// with `expires_on` unix seconds, or only `expiresOn` in local time on older
/// CLIs).
pub(crate) fn parse_token_response(body: &str, now_unix: i64) -> anyhow::Result<(String, Duration)> {
    let v: serde_json::Value =
        serde_json::from_str(body).context("token response is not JSON")?;
    let token = v["access_token"]
        .as_str()
        .or_else(|| v["accessToken"].as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("token response carried no access token"))?
        .to_string();
    let secs = if let Some(n) = number_or_string(&v["expires_in"]) {
        Some(n)
    } else if let Some(on) = number_or_string(&v["expires_on"]) {
        Some(on - now_unix)
    } else {
        v["expiresOn"].as_str().and_then(|s| {
            use chrono::TimeZone;
            let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok()?;
            let local = chrono::Local.from_local_datetime(&naive).single()?;
            Some(local.timestamp() - now_unix)
        })
    };
    let lifetime = match secs {
        Some(s) => Duration::from_secs(s.max(0) as u64),
        None => UNSTATED_LIFETIME,
    };
    Ok((token, lifetime))
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// One cached token, refreshed ahead of expiry. The write lock is held across
/// the fetch so a burst of requests makes one token request.
#[derive(Default)]
struct TokenCache(tokio::sync::Mutex<Option<(String, Instant)>>);

impl TokenCache {
    async fn get<F, Fut>(&self, force: bool, fetch: F) -> anyhow::Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<(String, Duration)>>,
    {
        let mut slot = self.0.lock().await;
        if !force {
            if let Some((token, refresh_after)) = slot.as_ref() {
                if Instant::now() < *refresh_after {
                    return Ok(token.clone());
                }
            }
        }
        let (token, lifetime) = fetch().await?;
        let skew = EXPIRY_SKEW.min(lifetime / 2);
        *slot = Some((token.clone(), Instant::now() + lifetime.saturating_sub(skew)));
        Ok(token)
    }
}

async fn read_response(
    source_kind: &'static str,
    resp: reqwest::Response,
) -> anyhow::Result<(String, Duration)> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AzureTokenError { source_kind, status: status.as_u16(), body: body.trim().to_string() }.into());
    }
    parse_token_response(&body, now_unix())
}

// ── settings ────────────────────────────────────────────────────────────────

/// Everything a source needs, resolved once from config and the process
/// environment (config wins). Tests construct it directly.
#[derive(Debug, Clone)]
pub struct AzureSettings {
    pub provider: String,
    /// OAuth2 scope, e.g. `https://cognitiveservices.azure.com/.default`.
    pub scope: String,
    pub tenant_id: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub federated_token_file: Option<String>,
    pub authority_host: String,
    pub imds_host: String,
    /// App Service / Container Apps managed identity endpoint and header.
    pub identity_endpoint: Option<(String, String)>,
    pub cli_program: String,
    pub timeout: Duration,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn env(key: &str) -> Option<String> {
    non_empty(std::env::var(key).ok())
}

impl AzureSettings {
    pub fn from_config(provider: &str, config: &ProviderConfig, scope: &str, timeout: Duration) -> Self {
        let identity_endpoint = match (env("IDENTITY_ENDPOINT"), env("IDENTITY_HEADER")) {
            (Some(e), Some(h)) => Some((e, h)),
            _ => None,
        };
        Self {
            provider: provider.to_string(),
            scope: scope.to_string(),
            tenant_id: non_empty(config.azure_tenant_id.clone()).or_else(|| env("AZURE_TENANT_ID")),
            client_id: non_empty(config.azure_client_id.clone()).or_else(|| env("AZURE_CLIENT_ID")),
            client_secret: non_empty(config.azure_client_secret.clone()).or_else(|| env("AZURE_CLIENT_SECRET")),
            federated_token_file: non_empty(config.azure_federated_token_file.clone())
                .or_else(|| env("AZURE_FEDERATED_TOKEN_FILE")),
            authority_host: env("AZURE_AUTHORITY_HOST").unwrap_or_else(|| DEFAULT_AUTHORITY_HOST.to_string()),
            imds_host: env("AZURE_POD_IDENTITY_AUTHORITY_HOST").unwrap_or_else(|| DEFAULT_IMDS_HOST.to_string()),
            identity_endpoint,
            cli_program: "az".to_string(),
            timeout,
        }
    }

    fn http(&self) -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .context("failed to build HTTP client for Entra token acquisition")
    }

    fn token_url(&self, tenant: &str) -> String {
        format!("{}/{}/oauth2/v2.0/token", self.authority_host.trim_end_matches('/'), tenant)
    }

    fn require(&self, what: &str, missing: &[(&str, bool)]) -> anyhow::Result<()> {
        let names: Vec<&str> = missing.iter().filter(|(_, absent)| *absent).map(|(n, _)| *n).collect();
        if names.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "[providers.{}]: credential_source = \"{what}\" needs {} (config key or environment \
             variable)",
            self.provider,
            names.join(", ")
        )
    }
}

// ── sources ─────────────────────────────────────────────────────────────────

/// Behaviour shared by every concrete Azure source.
macro_rules! azure_source_common {
    ($kind:expr) => {
        fn kind(&self) -> String {
            $kind.label()
        }
        fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
            is_azure_reauth(err)
        }
        fn verdict(&self) -> CredentialVerdict {
            $kind.verdict(&self.provider)
        }
        fn expired_guidance(&self) -> (String, String) {
            $kind.expired_guidance(&self.provider)
        }
    };
}

enum MiEndpoint {
    AppService { url: String, header: String },
    Imds { url: String },
}

/// Managed identity: App Service style when `IDENTITY_ENDPOINT` is set,
/// otherwise IMDS.
pub struct ManagedIdentitySource {
    provider: String,
    http: reqwest::Client,
    endpoint: MiEndpoint,
    resource: String,
    client_id: Option<String>,
    cache: TokenCache,
}

impl ManagedIdentitySource {
    pub fn new(s: &AzureSettings) -> anyhow::Result<Self> {
        let endpoint = match &s.identity_endpoint {
            Some((url, header)) => MiEndpoint::AppService { url: url.clone(), header: header.clone() },
            None => MiEndpoint::Imds {
                url: format!("{}/metadata/identity/oauth2/token", s.imds_host.trim_end_matches('/')),
            },
        };
        Ok(Self {
            provider: s.provider.clone(),
            http: s.http()?,
            endpoint,
            resource: resource_for_scope(&s.scope).to_string(),
            client_id: s.client_id.clone(),
            cache: TokenCache::default(),
        })
    }

    async fn fetch(&self) -> anyhow::Result<(String, Duration)> {
        let mut query = vec![("resource", self.resource.as_str())];
        if let Some(id) = &self.client_id {
            query.push(("client_id", id.as_str()));
        }
        let (req, url) = match &self.endpoint {
            MiEndpoint::AppService { url, header } => {
                query.push(("api-version", APP_SERVICE_API_VERSION));
                (self.http.get(url).header("X-IDENTITY-HEADER", header), url)
            }
            MiEndpoint::Imds { url } => {
                query.push(("api-version", IMDS_API_VERSION));
                (self.http.get(url).header("Metadata", "true"), url)
            }
        };
        let resp = req.query(&query).send().await.with_context(|| {
            format!("managed-identity token request to {url} failed — no reachable managed identity endpoint")
        })?;
        read_response("managed identity", resp).await
    }
}

#[async_trait]
impl CredentialSource for ManagedIdentitySource {
    async fn token(&self) -> anyhow::Result<String> {
        self.cache.get(false, || self.fetch()).await
    }
    async fn force_refresh(&self) -> anyhow::Result<String> {
        self.cache.get(true, || self.fetch()).await
    }
    azure_source_common!(AzureKind::ManagedIdentity);
}

/// What an app-registration token request authenticates with.
enum ClientAuth {
    Secret(String),
    /// Path to a federated token file, re-read on every fetch because the
    /// platform rotates it.
    FederatedTokenFile(String),
}

/// Client-credentials flow against the Entra token endpoint: a client secret
/// or a federated token (workload identity).
pub struct AppRegistrationSource {
    provider: String,
    kind: AzureKind,
    http: reqwest::Client,
    token_url: String,
    client_id: String,
    scope: String,
    auth: ClientAuth,
    cache: TokenCache,
}

impl AppRegistrationSource {
    pub fn client_secret(s: &AzureSettings) -> anyhow::Result<Self> {
        s.require(
            "client-secret",
            &[
                ("azure_tenant_id / AZURE_TENANT_ID", s.tenant_id.is_none()),
                ("azure_client_id / AZURE_CLIENT_ID", s.client_id.is_none()),
                ("azure_client_secret / AZURE_CLIENT_SECRET", s.client_secret.is_none()),
            ],
        )?;
        Self::build(s, AzureKind::ClientSecret, ClientAuth::Secret(s.client_secret.clone().unwrap()))
    }

    pub fn workload_identity(s: &AzureSettings) -> anyhow::Result<Self> {
        s.require(
            "workload-identity",
            &[
                ("azure_tenant_id / AZURE_TENANT_ID", s.tenant_id.is_none()),
                ("azure_client_id / AZURE_CLIENT_ID", s.client_id.is_none()),
                (
                    "azure_federated_token_file / AZURE_FEDERATED_TOKEN_FILE",
                    s.federated_token_file.is_none(),
                ),
            ],
        )?;
        Self::build(
            s,
            AzureKind::WorkloadIdentity,
            ClientAuth::FederatedTokenFile(s.federated_token_file.clone().unwrap()),
        )
    }

    fn build(s: &AzureSettings, kind: AzureKind, auth: ClientAuth) -> anyhow::Result<Self> {
        Ok(Self {
            provider: s.provider.clone(),
            kind,
            http: s.http()?,
            token_url: s.token_url(s.tenant_id.as_deref().unwrap_or_default()),
            client_id: s.client_id.clone().unwrap_or_default(),
            scope: s.scope.clone(),
            auth,
            cache: TokenCache::default(),
        })
    }

    async fn fetch(&self) -> anyhow::Result<(String, Duration)> {
        let assertion;
        let mut form = vec![
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ];
        match &self.auth {
            ClientAuth::Secret(secret) => form.push(("client_secret", secret.as_str())),
            ClientAuth::FederatedTokenFile(path) => {
                assertion = std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read the federated token file {path}"))?;
                form.push(("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"));
                form.push(("client_assertion", assertion.trim()));
            }
        }
        let resp = self.http.post(&self.token_url).form(&form).send().await.with_context(|| {
            format!(
                "Entra token request to {} failed — check network egress to the login authority",
                self.token_url
            )
        })?;
        let label = match self.kind {
            AzureKind::WorkloadIdentity => "workload identity",
            _ => "client secret",
        };
        read_response(label, resp).await
    }
}

#[async_trait]
impl CredentialSource for AppRegistrationSource {
    async fn token(&self) -> anyhow::Result<String> {
        self.cache.get(false, || self.fetch()).await
    }
    async fn force_refresh(&self) -> anyhow::Result<String> {
        self.cache.get(true, || self.fetch()).await
    }
    fn kind(&self) -> String {
        self.kind.label()
    }
    fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
        is_azure_reauth(err)
    }
    fn verdict(&self) -> CredentialVerdict {
        self.kind.verdict(&self.provider)
    }
    fn expired_guidance(&self) -> (String, String) {
        self.kind.expired_guidance(&self.provider)
    }
}

/// `az account get-access-token`: a developer's own login.
pub struct CliSource {
    provider: String,
    program: String,
    resource: String,
    tenant_id: Option<String>,
    cache: TokenCache,
}

impl CliSource {
    pub fn new(s: &AzureSettings) -> Self {
        Self {
            provider: s.provider.clone(),
            program: s.cli_program.clone(),
            resource: resource_for_scope(&s.scope).to_string(),
            tenant_id: s.tenant_id.clone(),
            cache: TokenCache::default(),
        }
    }

    async fn fetch(&self) -> anyhow::Result<(String, Duration)> {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(["account", "get-access-token", "--resource", &self.resource, "--output", "json"]);
        if let Some(t) = &self.tenant_id {
            cmd.args(["--tenant", t]);
        }
        cmd.kill_on_drop(true);
        let out = tokio::time::timeout(CLI_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("Azure CLI did not answer within {}s", CLI_TIMEOUT.as_secs()))?
            .with_context(|| format!("could not run the Azure CLI ({}) — is it installed and on PATH?", self.program))?;
        if !out.status.success() {
            return Err(AzureCliError {
                code: out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "a signal".into()),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }
            .into());
        }
        parse_token_response(&String::from_utf8_lossy(&out.stdout), now_unix())
            .context("Azure CLI printed an unexpected token document")
    }
}

#[async_trait]
impl CredentialSource for CliSource {
    async fn token(&self) -> anyhow::Result<String> {
        self.cache.get(false, || self.fetch()).await
    }
    async fn force_refresh(&self) -> anyhow::Result<String> {
        self.cache.get(true, || self.fetch()).await
    }
    azure_source_common!(AzureKind::Cli);
}

fn build_kind(kind: AzureKind, s: &AzureSettings) -> anyhow::Result<Arc<dyn CredentialSource>> {
    Ok(match kind {
        AzureKind::ManagedIdentity => Arc::new(ManagedIdentitySource::new(s)?),
        AzureKind::WorkloadIdentity => Arc::new(AppRegistrationSource::workload_identity(s)?),
        AzureKind::ClientSecret => Arc::new(AppRegistrationSource::client_secret(s)?),
        AzureKind::Cli => Arc::new(CliSource::new(s)),
    })
}

/// `credential_source = "default"`: resolves on first use, in the Azure SDKs'
/// `DefaultAzureCredential` order, and keeps the result for the process.
pub struct DefaultSource {
    settings: AzureSettings,
    imds_probe_timeout: Duration,
    resolved: tokio::sync::Mutex<Option<(AzureKind, Arc<dyn CredentialSource>)>>,
    /// Mirror of `resolved`'s kind for the synchronous reporting methods.
    resolved_kind: Mutex<Option<AzureKind>>,
}

impl DefaultSource {
    pub fn new(settings: AzureSettings) -> Self {
        Self {
            settings,
            imds_probe_timeout: IMDS_PROBE_TIMEOUT,
            resolved: tokio::sync::Mutex::new(None),
            resolved_kind: Mutex::new(None),
        }
    }

    pub fn with_imds_probe_timeout(mut self, t: Duration) -> Self {
        self.imds_probe_timeout = t;
        self
    }

    fn kind_now(&self) -> Option<AzureKind> {
        *self.resolved_kind.lock().unwrap()
    }

    /// Returns the resolved source, plus a token when resolving already
    /// fetched one (the IMDS probe).
    async fn resolve(&self) -> anyhow::Result<(Arc<dyn CredentialSource>, Option<String>)> {
        let mut slot = self.resolved.lock().await;
        if let Some((_, source)) = slot.as_ref() {
            return Ok((source.clone(), None));
        }
        let s = &self.settings;
        let (kind, source, token) = if s.tenant_id.is_some() && s.client_id.is_some() && s.client_secret.is_some() {
            (AzureKind::ClientSecret, build_kind(AzureKind::ClientSecret, s)?, None)
        } else if s.tenant_id.is_some() && s.client_id.is_some() && s.federated_token_file.is_some() {
            (AzureKind::WorkloadIdentity, build_kind(AzureKind::WorkloadIdentity, s)?, None)
        } else if s.identity_endpoint.is_some() {
            (AzureKind::ManagedIdentity, build_kind(AzureKind::ManagedIdentity, s)?, None)
        } else {
            let imds = build_kind(AzureKind::ManagedIdentity, s)?;
            match tokio::time::timeout(self.imds_probe_timeout, imds.token()).await {
                Ok(Ok(token)) => (AzureKind::ManagedIdentity, imds, Some(token)),
                outcome => {
                    let why = match outcome {
                        Ok(Err(e)) => format!("{e:#}"),
                        _ => format!("no answer within {}ms", self.imds_probe_timeout.as_millis()),
                    };
                    tracing::info!(
                        provider = s.provider.as_str(),
                        reason = %why,
                        "azure default credential: managed identity not available, trying the Azure CLI"
                    );
                    (AzureKind::Cli, build_kind(AzureKind::Cli, s)?, None)
                }
            }
        };
        tracing::info!(
            provider = s.provider.as_str(),
            resolved = kind.label().as_str(),
            "azure default credential resolved"
        );
        *slot = Some((kind, source.clone()));
        *self.resolved_kind.lock().unwrap() = Some(kind);
        Ok((source, token))
    }
}

#[async_trait]
impl CredentialSource for DefaultSource {
    async fn token(&self) -> anyhow::Result<String> {
        match self.resolve().await? {
            (_, Some(token)) => Ok(token),
            (source, None) => source.token().await,
        }
    }

    async fn force_refresh(&self) -> anyhow::Result<String> {
        match self.resolve().await? {
            (_, Some(token)) => Ok(token),
            (source, None) => source.force_refresh().await,
        }
    }

    fn kind(&self) -> String {
        match self.kind_now() {
            Some(k) => format!("azure-default-{}", k.suffix()),
            None => "azure-default-unresolved".to_string(),
        }
    }

    fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
        is_azure_reauth(err)
    }

    fn verdict(&self) -> CredentialVerdict {
        match self.kind_now() {
            Some(k) => k.verdict(&self.settings.provider),
            None => CredentialVerdict::new(
                CredentialStatus::Unknown,
                "default Azure credential not resolved yet; it resolves on the first request",
                "",
            ),
        }
    }

    fn expired_guidance(&self) -> (String, String) {
        self.kind_now()
            .unwrap_or(AzureKind::Cli)
            .expired_guidance(&self.settings.provider)
    }

    /// A dead CLI login falls back to managed identity when its endpoint
    /// answers — the Azure analogue of a gcloud user login falling back to the
    /// GCE metadata server.
    fn fallback(&self) -> Option<FallbackFactory> {
        if self.kind_now() != Some(AzureKind::Cli) {
            return None;
        }
        let settings = self.settings.clone();
        Some(Arc::new(move || build_kind(AzureKind::ManagedIdentity, &settings)))
    }
}

// ── wiring ──────────────────────────────────────────────────────────────────

/// An Entra credential behind the shared chain, usable wherever an Azure
/// adapter wants a bearer token.
pub struct AzureCredential {
    chain: CredentialChain,
}

impl AzureCredential {
    pub fn new(chain: CredentialChain) -> Self {
        Self { chain }
    }

    /// Build for an explicit `credential_source`, or — `None` — the
    /// `environment` behaviour the Entra-by-default providers always had.
    pub fn build(source: Option<AzureCredentialSource>, settings: AzureSettings) -> anyhow::Result<Self> {
        let provider = settings.provider.clone();
        let scope = settings.scope.clone();
        let (label, primary): (&str, Arc<dyn CredentialSource>) = match source {
            Some(AzureCredentialSource::Default) => ("default", Arc::new(DefaultSource::new(settings))),
            Some(AzureCredentialSource::ManagedIdentity) => {
                ("managed-identity", build_kind(AzureKind::ManagedIdentity, &settings)?)
            }
            Some(AzureCredentialSource::WorkloadIdentity) => {
                ("workload-identity", build_kind(AzureKind::WorkloadIdentity, &settings)?)
            }
            Some(AzureCredentialSource::ClientSecret) => {
                ("client-secret", build_kind(AzureKind::ClientSecret, &settings)?)
            }
            Some(AzureCredentialSource::Cli) => ("cli", build_kind(AzureKind::Cli, &settings)?),
            None => ("environment", Self::environment(&settings)?),
        };
        tracing::info!(
            provider = provider.as_str(),
            credential_source = label,
            credential_kind = primary.kind().as_str(),
            scope = scope.as_str(),
            "azure: Entra credential configured"
        );
        Ok(Self::new(CredentialChain::new(provider, label, primary)))
    }

    /// The earlier unconfigured behaviour: the client secret from the
    /// environment when all three variables are present, else managed
    /// identity. A half-set trio is warned about, not failed: it is far more
    /// likely a deployment mistake than a request for managed identity.
    fn environment(s: &AzureSettings) -> anyhow::Result<Arc<dyn CredentialSource>> {
        if s.tenant_id.is_some() && s.client_id.is_some() && s.client_secret.is_some() {
            return build_kind(AzureKind::ClientSecret, s);
        }
        if s.tenant_id.is_some() || s.client_secret.is_some() {
            let missing: Vec<&str> = [
                ("AZURE_TENANT_ID", s.tenant_id.is_none()),
                ("AZURE_CLIENT_ID", s.client_id.is_none()),
                ("AZURE_CLIENT_SECRET", s.client_secret.is_none()),
            ]
            .iter()
            .filter(|(_, absent)| *absent)
            .map(|(n, _)| *n)
            .collect();
            tracing::warn!(
                missing = ?missing,
                "azure: some Entra client-secret variables are set but not all — falling back to \
                 managed identity. Set the missing variable(s) if an app registration was intended."
            );
        }
        build_kind(AzureKind::ManagedIdentity, s)
    }

    pub fn report(&self) -> CredentialReport {
        self.chain.report()
    }
}

#[async_trait]
impl TokenProvider for AzureCredential {
    async fn token(&self) -> anyhow::Result<String> {
        self.chain.token().await
    }

    fn credential_report(&self) -> Option<CredentialReport> {
        Some(self.chain.report())
    }
}

/// How an Azure request authenticates.
pub enum AzureAuth {
    /// A Microsoft Entra ID bearer token.
    Entra(Arc<dyn TokenProvider>),
    /// The `api-key` header.
    ApiKey(String),
}

impl AzureAuth {
    /// Entra-by-default providers (`foundry`, `bing_grounding`): a non-empty
    /// `api_key` opts into key auth; otherwise an Entra credential per
    /// `credential_source` (unset = `environment`).
    pub fn entra_by_default(
        provider: &str,
        config: &ProviderConfig,
        scope: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let source = config.azure_credential()?;
        if source.is_none() && !config.api_key.trim().is_empty() {
            tracing::info!(
                provider,
                "azure: `api_key` is set, so key auth (the `api-key` header) is used instead of \
                 Entra. Entra — managed identity, workload identity, or an app registration — is \
                 the intended mode; a key in config.toml is a secret at rest."
            );
            return Ok(Self::ApiKey(config.api_key.trim().to_string()));
        }
        let settings = AzureSettings::from_config(provider, config, scope, timeout);
        Ok(Self::Entra(Arc::new(AzureCredential::build(source, settings)?)))
    }

    /// Key-by-default providers (`azure`): key auth unless a
    /// `credential_source` is configured.
    pub fn key_by_default(
        provider: &str,
        config: &ProviderConfig,
        scope: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        match config.azure_credential()? {
            None => Ok(Self::ApiKey(config.api_key.clone())),
            Some(source) => {
                let settings = AzureSettings::from_config(provider, config, scope, timeout);
                Ok(Self::Entra(Arc::new(AzureCredential::build(Some(source), settings)?)))
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Entra(_) => "entra",
            Self::ApiKey(_) => "api_key",
        }
    }

    /// Attach credentials to an outgoing request. Async because the Entra mode
    /// may have to mint a token (cached in-process; usually a no-op).
    pub async fn apply(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(match self {
            Self::Entra(provider) => req.bearer_auth(provider.token().await?),
            Self::ApiKey(key) => req.header("api-key", key),
        })
    }

    pub fn credential_report(&self) -> Option<CredentialReport> {
        match self {
            Self::Entra(p) => p.credential_report(),
            Self::ApiKey(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
