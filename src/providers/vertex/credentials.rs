//! Credential-source policy for the Vertex provider: which Google credential
//! is active, whether a token failure is permanent, and when to fall back to
//! the metadata server.
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
//! [`CredentialChain`] turns that into one of two outcomes:
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
use crate::providers::credential_error::CredentialExpired;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Builds the metadata-server token source used as the reauth fallback.
pub(crate) type FallbackFactory =
    Arc<dyn Fn() -> anyhow::Result<Arc<dyn TokenProvider>> + Send + Sync>;

/// How long a fallback probe may take before we decide the metadata server is
/// not there. Off GCE `metadata.google.internal` usually fails DNS at once,
/// but a black-holed route would otherwise hang the request.
pub(crate) const FALLBACK_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum spacing between fallback probes, and how long a permanent failure
/// is answered from memory before the configured credential is tried again.
/// The latter keeps a broken credential from sending one refresh request to
/// Google per incoming call, while still noticing a re-login within a minute.
pub(crate) const RECHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct ChainState {
    /// Set once the metadata server has taken over; never unset.
    active_fallback: Option<Arc<dyn TokenProvider>>,
    last_probe: Option<Instant>,
    /// The last permanent failure and when it was seen.
    expired: Option<(Instant, CredentialExpired)>,
}

/// See the module docs.
pub(crate) struct CredentialChain {
    provider: String,
    source: ResolvedGcpCredential,
    primary: Arc<dyn TokenProvider>,
    primary_kind: Box<dyn Fn() -> CredentialKind + Send + Sync>,
    fallback: Option<FallbackFactory>,
    probe_timeout: Duration,
    recheck_interval: Duration,
    state: Mutex<ChainState>,
    /// Serialises fallback probes so a burst of failing requests makes one
    /// metadata-server call, not one each.
    probe_lock: tokio::sync::Mutex<()>,
}

impl CredentialChain {
    /// `fallback` is honoured only under ADC — an explicit `"metadata"` or
    /// service-account source means the operator chose that credential, and
    /// silently swapping it would defeat the choice.
    pub(crate) fn new(
        provider: impl Into<String>,
        source: ResolvedGcpCredential,
        primary: Arc<dyn TokenProvider>,
        primary_kind: impl Fn() -> CredentialKind + Send + Sync + 'static,
        fallback: Option<FallbackFactory>,
    ) -> Self {
        let fallback = match source {
            ResolvedGcpCredential::Adc => fallback,
            _ => None,
        };
        Self {
            provider: provider.into(),
            source,
            primary,
            primary_kind: Box::new(primary_kind),
            fallback,
            probe_timeout: FALLBACK_PROBE_TIMEOUT,
            recheck_interval: RECHECK_INTERVAL,
            state: Mutex::new(ChainState::default()),
            probe_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    fn with_timings(mut self, probe_timeout: Duration, recheck_interval: Duration) -> Self {
        self.probe_timeout = probe_timeout;
        self.recheck_interval = recheck_interval;
        self
    }

    /// The credential serving requests right now.
    pub(crate) fn active_kind(&self) -> CredentialKind {
        if self.fallback_active() {
            return CredentialKind::Metadata;
        }
        (self.primary_kind)()
    }

    pub(crate) fn fallback_active(&self) -> bool {
        self.state.lock().unwrap().active_fallback.is_some()
    }

    fn active_fallback(&self) -> Option<Arc<dyn TokenProvider>> {
        self.state.lock().unwrap().active_fallback.clone()
    }

    async fn fetch(&self, force: bool) -> anyhow::Result<String> {
        if let Some(fallback) = self.active_fallback() {
            let result = if force {
                fallback.force_rebuild_token().await
            } else {
                fallback.token().await
            };
            return result.map_err(|e| self.escalate(e, CredentialKind::Metadata));
        }

        if let Some(expired) = self.recent_expiry() {
            return Err(anyhow::Error::new(expired));
        }

        let result = if force {
            self.primary.force_rebuild_token().await
        } else {
            self.primary.token().await
        };
        let err = match result {
            Ok(token) => {
                self.state.lock().unwrap().expired = None;
                return Ok(token);
            }
            Err(err) => err,
        };
        if classify_token_error(&err) == TokenFailure::Transient {
            return Err(err);
        }
        if let Some(token) = self.try_fallback(&err).await {
            return Ok(token);
        }
        let expired = self.expired_error(&err, (self.primary_kind)());
        tracing::error!(
            provider = self.provider.as_str(),
            credential = expired.credential_kind.as_str(),
            error = %format!("{err:#}"),
            "credential needs reauthentication and no fallback is available; failing requests \
             with credential_expired (not retried, not counted toward the circuit breaker)"
        );
        self.state.lock().unwrap().expired = Some((Instant::now(), expired.clone()));
        Err(anyhow::Error::new(expired))
    }

    fn recent_expiry(&self) -> Option<CredentialExpired> {
        let state = self.state.lock().unwrap();
        match &state.expired {
            Some((at, expired)) if at.elapsed() < self.recheck_interval => Some(expired.clone()),
            _ => None,
        }
    }

    fn escalate(&self, err: anyhow::Error, kind: CredentialKind) -> anyhow::Error {
        match classify_token_error(&err) {
            TokenFailure::Transient => err,
            TokenFailure::NeedsReauth => anyhow::Error::new(self.expired_error(&err, kind)),
        }
    }

    fn expired_error(&self, err: &anyhow::Error, kind: CredentialKind) -> CredentialExpired {
        if let Some(existing) = crate::providers::credential_error::find_credential_expired(err) {
            return existing.clone();
        }
        let hint = match (kind, &self.source) {
            (CredentialKind::Metadata, _) => format!(
                "Check the service account attached to this instance and that it may call \
                 {} (IAM role and access scopes).",
                self.provider
            ),
            (_, ResolvedGcpCredential::ServiceAccountFile(_)) => {
                "Replace or re-enable the service-account key named by credentials_path."
                    .to_string()
            }
            _ => format!(
                "Reauthenticate: gcloud auth application-default login, or set \
                 credential_source = \"metadata\" under [providers.{}] to use the instance's \
                 attached service account.",
                self.provider
            ),
        };
        CredentialExpired {
            provider: self.provider.clone(),
            credential_kind: kind.as_str().to_string(),
            hint,
            detail: format!("{err:#}"),
        }
    }

    /// Try to switch to the metadata server after a reauth failure. Returns a
    /// token from it on success; `None` when there is no fallback, it was
    /// probed too recently, or it is unreachable.
    async fn try_fallback(&self, cause: &anyhow::Error) -> Option<String> {
        let factory = self.fallback.as_ref()?;
        let _probe = self.probe_lock.lock().await;

        // Another request switched over while we waited for the probe lock.
        if let Some(fallback) = self.active_fallback() {
            return fallback.token().await.ok();
        }
        {
            let mut state = self.state.lock().unwrap();
            if let Some(last) = state.last_probe {
                if last.elapsed() < self.recheck_interval {
                    return None;
                }
            }
            state.last_probe = Some(Instant::now());
        }

        let probe = async {
            let provider = factory()?;
            let token = provider.token().await?;
            Ok::<_, anyhow::Error>((provider, token))
        };
        match tokio::time::timeout(self.probe_timeout, probe).await {
            Ok(Ok((provider, token))) => {
                tracing::warn!(
                    provider = self.provider.as_str(),
                    from = (self.primary_kind)().as_str(),
                    to = CredentialKind::Metadata.as_str(),
                    cause = %format!("{cause:#}"),
                    "CREDENTIAL FALLBACK: the Application Default Credential needs interactive \
                     reauthentication; switching to the metadata server's attached service \
                     account for the rest of this process. Set credential_source = \"metadata\" \
                     to make this explicit, or reauthenticate and restart to go back."
                );
                let mut state = self.state.lock().unwrap();
                state.active_fallback = Some(provider);
                state.expired = None;
                Some(token)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    provider = self.provider.as_str(),
                    error = %format!("{e:#}"),
                    "metadata-server fallback is not available"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    provider = self.provider.as_str(),
                    timeout_secs = self.probe_timeout.as_secs(),
                    "metadata-server fallback did not answer in time"
                );
                None
            }
        }
    }
}

#[async_trait]
impl TokenProvider for CredentialChain {
    async fn token(&self) -> anyhow::Result<String> {
        self.fetch(false).await
    }

    async fn force_rebuild_token(&self) -> anyhow::Result<String> {
        self.fetch(true).await
    }

    fn credential_report(&self) -> Option<CredentialReport> {
        Some(CredentialReport {
            source: match self.source {
                ResolvedGcpCredential::Adc => "adc",
                ResolvedGcpCredential::Metadata => "metadata",
                ResolvedGcpCredential::ServiceAccountFile(_) => "explicit-file",
            },
            kind: self.active_kind(),
            fallback_active: self.fallback_active(),
        })
    }
}

/// What deep health reports about a provider's credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialReport {
    /// The configured source (`adc`, `metadata`, `explicit-file`).
    pub source: &'static str,
    /// The credential actually in use.
    pub kind: CredentialKind,
    /// Whether the metadata-server reauth fallback has taken over.
    pub fallback_active: bool,
}

impl CredentialReport {
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "source": self.source,
            "kind": self.kind.as_str(),
            "fallback_active": self.fallback_active,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn factory_for(provider: Arc<Scripted>, builds: Arc<AtomicUsize>) -> FallbackFactory {
        Arc::new(move || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(provider.clone() as Arc<dyn TokenProvider>)
        })
    }

    fn chain(
        source: ResolvedGcpCredential,
        primary: Arc<Scripted>,
        kind: CredentialKind,
        fallback: Option<FallbackFactory>,
    ) -> CredentialChain {
        CredentialChain::new("vertex", source, primary, move || kind, fallback)
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

    #[tokio::test]
    async fn healthy_primary_is_used_and_fallback_never_built() {
        let builds = Arc::new(AtomicUsize::new(0));
        let c = chain(
            ResolvedGcpCredential::Adc,
            Scripted::ok("user-token"),
            CredentialKind::AdcUser,
            Some(factory_for(Scripted::ok("mds-token"), builds.clone())),
        );
        assert_eq!(c.token().await.unwrap(), "user-token");
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        assert_eq!(c.active_kind(), CredentialKind::AdcUser);
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
            Some(factory_for(mds.clone(), builds.clone())),
        );

        assert_eq!(c.token().await.unwrap(), "mds-token");
        assert_eq!(c.active_kind(), CredentialKind::Metadata);
        assert!(c.fallback_active());

        // Later calls go straight to the metadata server.
        assert_eq!(c.token().await.unwrap(), "mds-token");
        assert_eq!(c.force_rebuild_token().await.unwrap(), "mds-token");
        assert_eq!(primary.calls(), 1, "primary must not be asked again after the switch");
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let report = c.credential_report().unwrap();
        assert_eq!(report.to_json()["kind"], "metadata");
        assert_eq!(report.to_json()["source"], "adc");
        assert_eq!(report.to_json()["fallback_active"], true);
    }

    #[tokio::test]
    async fn adc_reauth_without_a_metadata_server_is_credential_expired() {
        let c = chain(
            ResolvedGcpCredential::Adc,
            Scripted::err(REAUTH_BODY),
            CredentialKind::AdcUser,
            Some(factory_for(
                Scripted::err("dns error: metadata.google.internal"),
                Arc::new(AtomicUsize::new(0)),
            )),
        );
        let err = c.token().await.unwrap_err();
        let expired = crate::providers::credential_error::find_credential_expired(&err)
            .expect("must be the typed permanent error");
        assert_eq!(expired.credential_kind, "adc-user");
        assert!(expired.hint.contains("gcloud auth application-default login"), "{}", expired.hint);
        assert!(expired.hint.contains("credential_source = \"metadata\""), "{}", expired.hint);
        assert!(expired.detail.contains("invalid_rapt"), "{}", expired.detail);
        assert!(!c.fallback_active());
    }

    #[tokio::test]
    async fn a_hanging_metadata_server_does_not_hang_the_request() {
        struct Hangs;
        #[async_trait]
        impl TokenProvider for Hangs {
            async fn token(&self) -> anyhow::Result<String> {
                std::future::pending().await
            }
        }
        let factory: FallbackFactory = Arc::new(|| Ok(Arc::new(Hangs) as Arc<dyn TokenProvider>));
        let c = CredentialChain::new(
            "vertex",
            ResolvedGcpCredential::Adc,
            Scripted::err(REAUTH_BODY),
            || CredentialKind::AdcUser,
            Some(factory),
        )
        .with_timings(Duration::from_millis(20), Duration::ZERO);
        let err = c.token().await.unwrap_err();
        assert!(crate::providers::credential_error::find_credential_expired(&err).is_some());
    }

    #[tokio::test]
    async fn transient_failures_pass_through_without_fallback() {
        let builds = Arc::new(AtomicUsize::new(0));
        let c = chain(
            ResolvedGcpCredential::Adc,
            Scripted::err("error sending request: connection reset"),
            CredentialKind::AdcUser,
            Some(factory_for(Scripted::ok("mds-token"), builds.clone())),
        );
        let err = c.token().await.unwrap_err();
        assert!(crate::providers::credential_error::find_credential_expired(&err).is_none());
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
                Some(factory_for(Scripted::ok("mds-token"), builds.clone())),
            );
            let err = c.token().await.unwrap_err();
            let expired = crate::providers::credential_error::find_credential_expired(&err).unwrap();
            assert_eq!(expired.credential_kind, kind.as_str());
            assert!(expired.hint.contains(hint), "{}", expired.hint);
            assert_eq!(builds.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn a_permanent_failure_is_answered_from_memory_within_the_recheck_interval() {
        let primary = Scripted::err(REAUTH_BODY);
        let c = CredentialChain::new(
            "vertex",
            ResolvedGcpCredential::Adc,
            primary.clone(),
            || CredentialKind::AdcUser,
            None,
        )
        .with_timings(Duration::from_secs(1), Duration::from_secs(3600));
        for _ in 0..5 {
            let err = c.token().await.unwrap_err();
            assert!(crate::providers::credential_error::find_credential_expired(&err).is_some());
        }
        assert_eq!(primary.calls(), 1, "a known-dead credential must not be refreshed per request");
    }

    #[tokio::test]
    async fn fallback_probes_are_spaced_by_the_recheck_interval() {
        let builds = Arc::new(AtomicUsize::new(0));
        let c = CredentialChain::new(
            "vertex",
            ResolvedGcpCredential::Adc,
            Scripted::err(REAUTH_BODY),
            || CredentialKind::AdcUser,
            Some(factory_for(Scripted::err("no metadata server"), builds.clone())),
        )
        // The second call lands inside the interval: it must neither probe
        // the metadata server again nor refresh the dead credential.
        .with_timings(Duration::from_secs(1), Duration::from_secs(3600));
        c.token().await.unwrap_err();
        c.force_rebuild_token().await.unwrap_err();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }
}
