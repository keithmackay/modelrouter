//! Google credentials as a [`CredentialSource`]: which Google credential is
//! active, whether a token failure is permanent, what health should say about
//! it, and when to fall back to the metadata server. The shared failure policy
//! (typed `credential_expired`, dead-credential cache, spaced fallback probes,
//! health report) lives in `providers::credentials`.
//!
//! Why this layer exists. Application Default Credentials checks the gcloud
//! user file (`~/.config/gcloud/application_default_credentials.json`) BEFORE
//! the GCE metadata server. On a VM that is meant to run as its attached
//! service account, one `gcloud auth application-default login` therefore
//! silently moves the proxy onto a personal credential. Google's reauth
//! policy later refuses to refresh that credential (`invalid_grant`,
//! `invalid_rapt`, "reauth related error") until a human logs in again.
//! [`super::auth::RebuildingProvider`] cannot fix that — rebuilding re-reads
//! the same refused refresh token — and before this layer every failure
//! counted toward the circuit breaker, so callers saw a transient-looking
//! "circuit breaker open" and retried for hours.
//!
//! [`GcpSource`], run through the provider-neutral `CredentialChain`, turns
//! that into one of two outcomes:
//! - under `credential_source = "adc"`, a reachable metadata server takes over
//!   for the rest of the process, with a loud WARN naming the switch;
//! - otherwise a [`CredentialExpired`] error, which the retry loop and circuit
//!   breaker step aside for and the API maps to 401 `credential_expired`.
//!
//! Transient failures (network errors, 5xx from the token endpoint) pass
//! through untouched and keep their existing retry/breaker behaviour.
//!
//! Every token source here is an `Arc<dyn TokenProvider>` and the fallback is
//! an injected factory, so the policy is tested with fakes and never talks to
//! Google.

use super::auth::TokenProvider;
use crate::config::schema::ResolvedGcpCredential;
use crate::providers::credentials::{
    CredentialSource, CredentialStatus, CredentialVerdict, FallbackFactory,
};
use async_trait::async_trait;
use std::sync::Arc;

/// Which credential is actually serving requests. Reported by deep health;
/// names the credential type only, never any credential material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// ADC resolved to a gcloud user login (`authorized_user`).
    AdcUser,
    /// ADC resolved to a service-account key file.
    AdcServiceAccount,
    /// ADC resolved to some other file type (workload identity federation,
    /// impersonation, ...).
    AdcOther,
    /// The GCE/GKE/Cloud Run metadata server — configured directly, reached
    /// through ADC because no credential file exists, or taken over after a
    /// reauth failure.
    Metadata,
    /// The service-account file named by `credentials_path`.
    ExplicitFile,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AdcUser => "adc-user",
            Self::AdcServiceAccount => "adc-service-account",
            Self::AdcOther => "adc-other",
            Self::Metadata => "metadata",
            Self::ExplicitFile => "explicit-file",
        }
    }

    /// The router's judgment of this credential type while it works.
    pub fn verdict(self, provider: &str) -> CredentialVerdict {
        match self {
            Self::AdcUser => CredentialVerdict::new(
                CredentialStatus::Warn,
                "personal login; expires under the identity provider's reauthentication policy",
                format!(
                    "set credential_source = \"metadata\" under [providers.{provider}] to use the \
                     instance's attached service account"
                ),
            ),
            Self::AdcServiceAccount => {
                CredentialVerdict::ok("service-account key found through Application Default Credentials")
            }
            Self::AdcOther => CredentialVerdict::new(
                CredentialStatus::Unknown,
                "Application Default Credentials file of a type the router does not classify \
                 (workload identity federation, impersonation, ...)",
                "",
            ),
            Self::Metadata => CredentialVerdict::ok("the instance's attached service account"),
            Self::ExplicitFile => CredentialVerdict::ok("service-account key named by credentials_path"),
        }
    }
}

/// Map the `type` field of an ADC JSON file to a [`CredentialKind`].
pub(crate) fn kind_from_adc_json(raw: &str) -> CredentialKind {
    let kind = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string));
    match kind.as_deref() {
        Some("authorized_user") => CredentialKind::AdcUser,
        Some("service_account") => CredentialKind::AdcServiceAccount,
        _ => CredentialKind::AdcOther,
    }
}

/// Which credential ADC will resolve to, following the same order as
/// `google-cloud-auth`: `GOOGLE_APPLICATION_CREDENTIALS`, then the gcloud
/// well-known file under `home`, then the metadata server.
pub(crate) fn detect_adc_kind_with(env_path: Option<&str>, home: Option<&str>) -> CredentialKind {
    if let Some(path) = env_path {
        return std::fs::read_to_string(path)
            .map(|raw| kind_from_adc_json(&raw))
            .unwrap_or(CredentialKind::AdcOther);
    }
    let Some(home) = home else {
        return CredentialKind::Metadata;
    };
    let well_known = format!("{home}/.config/gcloud/application_default_credentials.json");
    match std::fs::read_to_string(well_known) {
        Ok(raw) => kind_from_adc_json(&raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CredentialKind::Metadata,
        Err(_) => CredentialKind::AdcOther,
    }
}

pub(crate) fn detect_adc_kind() -> CredentialKind {
    detect_adc_kind_with(
        std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Whether a failed token fetch can be fixed by trying again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenFailure {
    /// The identity provider refused the credential itself and will keep
    /// refusing it until a human reauthenticates or the config changes.
    NeedsReauth,
    /// Anything else — network trouble, a 5xx from the token endpoint, a
    /// credential file that failed to parse. Keeps the existing behaviour.
    Transient,
}

/// Markers of a refresh the identity provider will never grant again. Google
/// returns these in the token endpoint's JSON body, which `google-cloud-auth`
/// embeds in its error message (`..., body=<{"error":"invalid_grant",...}>`).
const REAUTH_MARKERS: &[&str] = &["invalid_grant", "invalid_rapt", "reauth"];

pub(crate) fn classify_token_error(err: &anyhow::Error) -> TokenFailure {
    if crate::providers::credential_error::find_credential_expired(err).is_some() {
        return TokenFailure::NeedsReauth;
    }
    let text = format!("{err:#}").to_ascii_lowercase();
    if REAUTH_MARKERS.iter().any(|m| text.contains(m)) {
        TokenFailure::NeedsReauth
    } else {
        TokenFailure::Transient
    }
}

/// A Google credential as seen by the provider-neutral chain.
///
/// `primary` is the rebuilding `google-cloud-auth` source for the configured
/// credential; `fallback` (kept only under ADC — an explicit `"metadata"` or
/// service-account source means the operator chose that credential, and
/// silently swapping it would defeat the choice) builds the metadata-server
/// source.
pub(crate) struct GcpSource {
    provider: String,
    configured: ResolvedGcpCredential,
    primary: Arc<dyn TokenProvider>,
    kind: Box<dyn Fn() -> CredentialKind + Send + Sync>,
    fallback: Option<FallbackFactory>,
}

impl GcpSource {
    pub(crate) fn new(
        provider: impl Into<String>,
        configured: ResolvedGcpCredential,
        primary: Arc<dyn TokenProvider>,
        kind: impl Fn() -> CredentialKind + Send + Sync + 'static,
        fallback: Option<FallbackFactory>,
    ) -> Self {
        let fallback = match configured {
            ResolvedGcpCredential::Adc => fallback,
            _ => None,
        };
        Self { provider: provider.into(), configured, primary, kind: Box::new(kind), fallback }
    }

    /// The metadata-server source: what `configured = Metadata` uses and what
    /// ADC falls back to.
    pub(crate) fn metadata(provider: impl Into<String>, primary: Arc<dyn TokenProvider>) -> Self {
        Self::new(provider, ResolvedGcpCredential::Metadata, primary, || CredentialKind::Metadata, None)
    }

    /// The configured source as health reports it.
    pub(crate) fn configured_label(configured: &ResolvedGcpCredential) -> &'static str {
        match configured {
            ResolvedGcpCredential::Adc => "adc",
            ResolvedGcpCredential::Metadata => "metadata",
            ResolvedGcpCredential::ServiceAccountFile(_) => "explicit-file",
        }
    }
}

#[async_trait]
impl CredentialSource for GcpSource {
    async fn token(&self) -> anyhow::Result<String> {
        self.primary.token().await
    }

    async fn force_refresh(&self) -> anyhow::Result<String> {
        self.primary.force_rebuild_token().await
    }

    fn kind(&self) -> String {
        (self.kind)().as_str().to_string()
    }

    fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
        classify_token_error(err) == TokenFailure::NeedsReauth
    }

    fn verdict(&self) -> CredentialVerdict {
        (self.kind)().verdict(&self.provider)
    }

    fn expired_guidance(&self) -> (String, String) {
        match ((self.kind)(), &self.configured) {
            (CredentialKind::Metadata, _) => (
                "the instance's attached service account was refused".to_string(),
                format!(
                    "Check the service account attached to this instance and that it may call \
                     {} (IAM role and access scopes).",
                    self.provider
                ),
            ),
            (_, ResolvedGcpCredential::ServiceAccountFile(_)) => (
                "the service-account key was refused".to_string(),
                "Replace or re-enable the service-account key named by credentials_path.".to_string(),
            ),
            _ => (
                "the Application Default Credential can no longer be refreshed without \
                 interactive reauthentication"
                    .to_string(),
                format!(
                    "Reauthenticate: gcloud auth application-default login, or set \
                     credential_source = \"metadata\" under [providers.{}] to use the instance's \
                     attached service account.",
                    self.provider
                ),
            ),
        }
    }

    fn fallback(&self) -> Option<FallbackFactory> {
        self.fallback.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::credential_error::find_credential_expired;
    use crate::providers::credentials::CredentialChain;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const REAUTH_BODY: &str = r#"failed to refresh user access token, body=<{"error":"invalid_grant","error_description":"reauth related error (invalid_rapt)","error_subtype":"invalid_rapt"}>"#;

    /// A token source scripted per call: `Ok(token)` or `Err(message)`.
    struct Scripted {
        result: Result<String, String>,
        calls: AtomicUsize,
    }

    impl Scripted {
        fn ok(token: &str) -> Arc<Self> {
            Arc::new(Self { result: Ok(token.into()), calls: AtomicUsize::new(0) })
        }
        fn err(message: &str) -> Arc<Self> {
            Arc::new(Self { result: Err(message.into()), calls: AtomicUsize::new(0) })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl TokenProvider for Scripted {
        async fn token(&self) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone().map_err(anyhow::Error::msg)
        }
    }

    fn metadata_factory(provider: Arc<Scripted>, builds: Arc<AtomicUsize>) -> FallbackFactory {
        Arc::new(move || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(GcpSource::metadata("vertex", provider.clone())) as Arc<dyn CredentialSource>)
        })
    }

    fn chain(
        source: ResolvedGcpCredential,
        primary: Arc<Scripted>,
        kind: CredentialKind,
        fallback: Option<FallbackFactory>,
    ) -> CredentialChain {
        let label = GcpSource::configured_label(&source);
        let gcp = GcpSource::new("vertex", source, primary, move || kind, fallback);
        CredentialChain::new("vertex", label, Arc::new(gcp))
            .with_timings(Duration::from_secs(1), Duration::ZERO)
    }

    #[test]
    fn reauth_errors_are_permanent() {
        for msg in [
            REAUTH_BODY,
            r#"body=<{"error":"invalid_grant","error_description":"Token has been expired or revoked."}>"#,
            "Reauthentication required",
        ] {
            assert_eq!(classify_token_error(&anyhow::anyhow!("{msg}")), TokenFailure::NeedsReauth, "{msg}");
        }
    }

    #[test]
    fn network_and_5xx_errors_are_transient() {
        for msg in [
            "failed to refresh user access token: error sending request: connection refused",
            "failed to refresh user access token, body=<Service Unavailable>: 503",
            "dns error: failed to lookup address information",
        ] {
            assert_eq!(classify_token_error(&anyhow::anyhow!("{msg}")), TokenFailure::Transient, "{msg}");
        }
    }

    #[test]
    fn classification_sees_through_context() {
        use anyhow::Context;
        let err = Err::<(), _>(anyhow::anyhow!("{REAUTH_BODY}"))
            .context("failed to fetch GCP access token")
            .unwrap_err();
        assert_eq!(classify_token_error(&err), TokenFailure::NeedsReauth);
    }

    #[test]
    fn adc_kind_follows_google_resolution_order() {
        let dir = std::env::temp_dir().join(format!("mr-adc-kind-{}", std::process::id()));
        let gcloud = dir.join(".config/gcloud");
        std::fs::create_dir_all(&gcloud).unwrap();
        let home = dir.to_str().unwrap();

        // No file anywhere: ADC lands on the metadata server.
        assert_eq!(detect_adc_kind_with(None, Some(home)), CredentialKind::Metadata);

        let well_known = gcloud.join("application_default_credentials.json");
        std::fs::write(&well_known, r#"{"type":"authorized_user","refresh_token":"x"}"#).unwrap();
        assert_eq!(detect_adc_kind_with(None, Some(home)), CredentialKind::AdcUser);

        // GOOGLE_APPLICATION_CREDENTIALS wins over the well-known file.
        let sa = dir.join("sa.json");
        std::fs::write(&sa, r#"{"type":"service_account"}"#).unwrap();
        assert_eq!(
            detect_adc_kind_with(Some(sa.to_str().unwrap()), Some(home)),
            CredentialKind::AdcServiceAccount
        );

        std::fs::write(&well_known, r#"{"type":"external_account"}"#).unwrap();
        assert_eq!(detect_adc_kind_with(None, Some(home)), CredentialKind::AdcOther);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_kind_maps_to_a_verdict() {
        use CredentialStatus::*;
        for (kind, status) in [
            (CredentialKind::AdcUser, Warn),
            (CredentialKind::AdcServiceAccount, Ok),
            (CredentialKind::AdcOther, Unknown),
            (CredentialKind::Metadata, Ok),
            (CredentialKind::ExplicitFile, Ok),
        ] {
            let v = kind.verdict("vertex");
            assert_eq!(v.status, status, "{kind:?}");
            assert!(!v.reason.is_empty(), "{kind:?}");
        }
        let user = CredentialKind::AdcUser.verdict("vertex");
        assert!(user.reason.contains("personal login"), "{}", user.reason);
        assert!(user.remediation.contains("credential_source = \"metadata\""), "{}", user.remediation);
    }

    #[tokio::test]
    async fn healthy_adc_user_is_used_reported_warn_and_fallback_never_built() {
        let builds = Arc::new(AtomicUsize::new(0));
        let c = chain(
            ResolvedGcpCredential::Adc,
            Scripted::ok("user-token"),
            CredentialKind::AdcUser,
            Some(metadata_factory(Scripted::ok("mds-token"), builds.clone())),
        );
        assert_eq!(c.token().await.unwrap(), "user-token");
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        let json = c.report().to_json();
        assert_eq!(json["kind"], "adc-user");
        assert_eq!(json["status"], "warn");
        assert_eq!(json["source"], "adc");
    }

    #[tokio::test]
    async fn adc_reauth_failure_falls_back_to_metadata_and_stays_there() {
        let primary = Scripted::err(REAUTH_BODY);
        let mds = Scripted::ok("mds-token");
        let builds = Arc::new(AtomicUsize::new(0));
        let c = chain(
            ResolvedGcpCredential::Adc,
            primary.clone(),
            CredentialKind::AdcUser,
            Some(metadata_factory(mds.clone(), builds.clone())),
        );

        assert_eq!(c.token().await.unwrap(), "mds-token");
        assert!(c.fallback_active());
        assert_eq!(c.token().await.unwrap(), "mds-token");
        assert_eq!(c.force_refresh().await.unwrap(), "mds-token");
        assert_eq!(primary.calls(), 1, "primary must not be asked again after the switch");
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let json = c.report().to_json();
        assert_eq!(json["kind"], "metadata");
        assert_eq!(json["source"], "adc");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["fallback_active"], true);
    }

    #[tokio::test]
    async fn adc_reauth_without_a_metadata_server_is_credential_expired() {
        let c = chain(
            ResolvedGcpCredential::Adc,
            Scripted::err(REAUTH_BODY),
            CredentialKind::AdcUser,
            Some(metadata_factory(
                Scripted::err("dns error: metadata.google.internal"),
                Arc::new(AtomicUsize::new(0)),
            )),
        );
        let err = c.token().await.unwrap_err();
        let expired = find_credential_expired(&err).expect("must be the typed permanent error");
        assert_eq!(expired.credential_kind, "adc-user");
        assert!(expired.remediation.contains("gcloud auth application-default login"), "{}", expired.remediation);
        assert!(expired.remediation.contains("credential_source = \"metadata\""), "{}", expired.remediation);
        assert!(expired.reason.contains("reauthentication"), "{}", expired.reason);
        assert!(expired.detail.contains("invalid_rapt"), "{}", expired.detail);
        assert!(!c.fallback_active());
        assert_eq!(c.report().to_json()["status"], "expired");
    }

    #[tokio::test]
    async fn transient_failures_pass_through_without_fallback() {
        let builds = Arc::new(AtomicUsize::new(0));
        let c = chain(
            ResolvedGcpCredential::Adc,
            Scripted::err("error sending request: connection reset"),
            CredentialKind::AdcUser,
            Some(metadata_factory(Scripted::ok("mds-token"), builds.clone())),
        );
        let err = c.token().await.unwrap_err();
        assert!(find_credential_expired(&err).is_none());
        assert!(err.to_string().contains("connection reset"));
        assert_eq!(builds.load(Ordering::SeqCst), 0, "a transient error must not trigger fallback");
    }

    #[tokio::test]
    async fn explicit_sources_never_fall_back() {
        for (source, kind, hint) in [
            (
                ResolvedGcpCredential::ServiceAccountFile("/x.json".into()),
                CredentialKind::ExplicitFile,
                "credentials_path",
            ),
            (ResolvedGcpCredential::Metadata, CredentialKind::Metadata, "attached to this instance"),
        ] {
            let builds = Arc::new(AtomicUsize::new(0));
            let c = chain(
                source,
                Scripted::err(REAUTH_BODY),
                kind,
                Some(metadata_factory(Scripted::ok("mds-token"), builds.clone())),
            );
            let err = c.token().await.unwrap_err();
            let expired = find_credential_expired(&err).unwrap();
            assert_eq!(expired.credential_kind, kind.as_str());
            assert!(expired.remediation.contains(hint), "{}", expired.remediation);
            assert_eq!(builds.load(Ordering::SeqCst), 0);
        }
    }
}
