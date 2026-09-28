//! Provider-neutral credential sources: the one place that decides what a
//! token failure means, when to fall back to another credential, and how a
//! credential's health is reported.
//!
//! Why this layer exists. Cloud identity SDKs resolve a "default" credential
//! from the environment, and on a developer-touched host that resolution can
//! land on a personal login (a gcloud user file, an Azure CLI session) instead
//! of the workload identity the host is meant to run as. Those logins expire
//! under the identity provider's reauthentication policy, and once they do,
//! every refresh is refused until a human logs in again. Retrying cannot fix
//! that, yet a plain provider error counts toward the circuit breaker, so
//! callers used to see a transient-looking "circuit breaker open" and retried
//! for hours.
//!
//! A provider plugs in by implementing [`CredentialSource`] for each
//! credential it can use. [`CredentialChain`] then supplies, identically for
//! every provider:
//! - the permanent-failure error ([`CredentialExpired`], HTTP 401
//!   `credential_expired`, not retried, not counted by the circuit breaker);
//! - a dead-credential cache: a permanent failure is answered from memory for
//!   [`RECHECK_INTERVAL`] instead of sending one refresh per request;
//! - an optional fallback: when the source offers one, a reauth failure probes
//!   it (bounded by [`FALLBACK_PROBE_TIMEOUT`], at most once per interval) and,
//!   if it answers, switches to it for the rest of the process with a loud
//!   WARN;
//! - the health report ([`CredentialReport`]), including a provider-neutral
//!   verdict a caller can act on without knowing which cloud is behind the
//!   router.
//!
//! Transient failures (network errors, 5xx and 429 from a token endpoint)
//! pass through untouched and keep the normal retry/breaker behaviour.
//!
//! Implementations today: `providers::vertex::credentials` (Google: ADC,
//! metadata server, service-account file) and `providers::azure_credentials`
//! (Microsoft Entra ID: managed identity, workload identity, client secret,
//! Azure CLI, and a default chain).
//!
//! Not feature-gated: the config validator, the API error mapping and the
//! health route use it whether or not any provider implementing it is
//! compiled in.

use crate::providers::credential_error::{find_credential_expired, CredentialExpired};
use async_trait::async_trait;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How a credential looks from the outside. Callers can act on this alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStatus {
    /// A non-expiring workload credential (instance identity, workload
    /// identity, a service-account key).
    Ok,
    /// Working now, but of a type that will need a human later (a personal
    /// login, a client secret with a fixed expiry).
    Warn,
    /// Dead: the identity provider refuses it until a human acts.
    Expired,
    /// Not yet known (for example a default chain that has not resolved).
    Unknown,
}

impl CredentialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Expired => "expired",
            Self::Unknown => "unknown",
        }
    }
}

/// A source's judgment of its own credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialVerdict {
    pub status: CredentialStatus,
    /// Short human text: what this credential is and why it has that status.
    pub reason: String,
    /// What an operator should do. Empty when nothing needs doing.
    pub remediation: String,
}

impl CredentialVerdict {
    pub fn new(
        status: CredentialStatus,
        reason: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            status,
            reason: reason.into(),
            remediation: remediation.into(),
        }
    }
    pub fn ok(reason: impl Into<String>) -> Self {
        Self::new(CredentialStatus::Ok, reason, "")
    }
}

/// Builds the credential to switch to after a reauth failure.
pub type FallbackFactory = Arc<dyn Fn() -> anyhow::Result<Arc<dyn CredentialSource>> + Send + Sync>;

/// One way of obtaining a bearer token for a provider.
///
/// Implementations own their token caching and their classification of
/// errors; everything that should behave the same for every provider lives in
/// [`CredentialChain`].
#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// A bearer token, from cache when still fresh.
    async fn token(&self) -> anyhow::Result<String>;

    /// Discard cached credential material and fetch again. Used after an
    /// upstream 401 that suggests the cached token went bad.
    async fn force_refresh(&self) -> anyhow::Result<String> {
        self.token().await
    }

    /// Stable label of the credential in use, e.g. `adc-user` or
    /// `azure-managed-identity`. Names the type only, never material.
    fn kind(&self) -> String;

    /// Whether `err` (from this source) means the credential is dead until a
    /// human acts. Anything else is treated as transient.
    fn is_reauth_required(&self, err: &anyhow::Error) -> bool;

    /// The verdict while the credential works.
    fn verdict(&self) -> CredentialVerdict;

    /// `(reason, remediation)` once it has died.
    fn expired_guidance(&self) -> (String, String);

    /// A credential to switch to when this one needs reauthentication. `None`
    /// (the default) when there is none, or when the operator explicitly chose
    /// this credential and silently swapping it would defeat the choice.
    fn fallback(&self) -> Option<FallbackFactory> {
        None
    }
}

/// How long a fallback probe may take before the fallback is declared absent.
/// A link-local metadata address off-platform usually fails at once, but a
/// black-holed route would otherwise hang the request.
pub const FALLBACK_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum spacing between fallback probes, and how long a permanent failure
/// is answered from memory before the configured credential is tried again.
/// Keeps a dead credential from sending one refresh per incoming call, while
/// still noticing a re-login within a minute.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct ChainState {
    /// Set once a fallback has taken over; never unset.
    active_fallback: Option<Arc<dyn CredentialSource>>,
    last_probe: Option<Instant>,
    /// The last permanent failure and when it was seen. Cleared by the next
    /// successful fetch.
    expired: Option<(Instant, CredentialExpired)>,
}

/// Applies the shared failure policy to a provider's [`CredentialSource`]. See
/// the module docs.
pub struct CredentialChain {
    provider: String,
    /// The configured source as the operator wrote it (or the provider's name
    /// for its default), reported by health.
    configured: String,
    primary: Arc<dyn CredentialSource>,
    probe_timeout: Duration,
    recheck_interval: Duration,
    state: Mutex<ChainState>,
    /// Serialises fallback probes so a burst of failing requests makes one
    /// probe, not one each.
    probe_lock: tokio::sync::Mutex<()>,
}

impl CredentialChain {
    pub fn new(
        provider: impl Into<String>,
        configured: impl Into<String>,
        primary: Arc<dyn CredentialSource>,
    ) -> Self {
        Self {
            provider: provider.into(),
            configured: configured.into(),
            primary,
            probe_timeout: FALLBACK_PROBE_TIMEOUT,
            recheck_interval: RECHECK_INTERVAL,
            state: Mutex::new(ChainState::default()),
            probe_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Override the probe timeout and recheck interval (tests).
    pub fn with_timings(mut self, probe_timeout: Duration, recheck_interval: Duration) -> Self {
        self.probe_timeout = probe_timeout;
        self.recheck_interval = recheck_interval;
        self
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub async fn token(&self) -> anyhow::Result<String> {
        self.fetch(false).await
    }

    pub async fn force_refresh(&self) -> anyhow::Result<String> {
        self.fetch(true).await
    }

    /// The source serving requests right now.
    fn active(&self) -> Arc<dyn CredentialSource> {
        self.active_fallback()
            .unwrap_or_else(|| self.primary.clone())
    }

    pub fn fallback_active(&self) -> bool {
        self.state.lock().unwrap().active_fallback.is_some()
    }

    fn active_fallback(&self) -> Option<Arc<dyn CredentialSource>> {
        self.state.lock().unwrap().active_fallback.clone()
    }

    /// What health reports for this credential.
    pub fn report(&self) -> CredentialReport {
        let active = self.active();
        let expired = self
            .state
            .lock()
            .unwrap()
            .expired
            .as_ref()
            .map(|(_, e)| e.clone());
        let verdict = match expired {
            Some(e) => CredentialVerdict::new(CredentialStatus::Expired, e.reason, e.remediation),
            None => active.verdict(),
        };
        CredentialReport {
            provider: self.provider.clone(),
            source: self.configured.clone(),
            kind: active.kind(),
            fallback_active: self.fallback_active(),
            verdict,
        }
    }

    async fn fetch(&self, force: bool) -> anyhow::Result<String> {
        if let Some(fallback) = self.active_fallback() {
            let result = if force {
                fallback.force_refresh().await
            } else {
                fallback.token().await
            };
            return result.map_err(|e| self.escalate(e, fallback.as_ref()));
        }

        if let Some(expired) = self.recent_expiry() {
            return Err(anyhow::Error::new(expired));
        }

        let result = if force {
            self.primary.force_refresh().await
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
        if !self.is_permanent(&err, self.primary.as_ref()) {
            return Err(err);
        }
        if let Some(token) = self.try_fallback(&err).await {
            return Ok(token);
        }
        let expired = self.expired_error(&err, self.primary.as_ref());
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

    fn is_permanent(&self, err: &anyhow::Error, source: &dyn CredentialSource) -> bool {
        find_credential_expired(err).is_some() || source.is_reauth_required(err)
    }

    fn recent_expiry(&self) -> Option<CredentialExpired> {
        let state = self.state.lock().unwrap();
        match &state.expired {
            Some((at, expired)) if at.elapsed() < self.recheck_interval => Some(expired.clone()),
            _ => None,
        }
    }

    fn escalate(&self, err: anyhow::Error, source: &dyn CredentialSource) -> anyhow::Error {
        if !self.is_permanent(&err, source) {
            return err;
        }
        let expired = self.expired_error(&err, source);
        self.state.lock().unwrap().expired = Some((Instant::now(), expired.clone()));
        anyhow::Error::new(expired)
    }

    fn expired_error(
        &self,
        err: &anyhow::Error,
        source: &dyn CredentialSource,
    ) -> CredentialExpired {
        if let Some(existing) = find_credential_expired(err) {
            return existing.clone();
        }
        let (reason, remediation) = source.expired_guidance();
        CredentialExpired {
            provider: self.provider.clone(),
            credential_kind: source.kind(),
            reason,
            remediation,
            detail: format!("{err:#}"),
        }
    }

    /// Try to switch to the source's fallback after a reauth failure. Returns
    /// a token from it on success; `None` when there is no fallback, it was
    /// probed too recently, or it did not answer.
    async fn try_fallback(&self, cause: &anyhow::Error) -> Option<String> {
        let factory = self.primary.fallback()?;
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
            let source = factory()?;
            let token = source.token().await?;
            Ok::<_, anyhow::Error>((source, token))
        };
        match tokio::time::timeout(self.probe_timeout, probe).await {
            Ok(Ok((source, token))) => {
                tracing::warn!(
                    provider = self.provider.as_str(),
                    from = self.primary.kind().as_str(),
                    to = source.kind().as_str(),
                    cause = %format!("{cause:#}"),
                    "CREDENTIAL FALLBACK: the configured credential needs interactive \
                     reauthentication; switching to the host's workload identity for the rest of \
                     this process. Configure that credential explicitly to make this permanent, \
                     or reauthenticate and restart to go back."
                );
                let mut state = self.state.lock().unwrap();
                state.active_fallback = Some(source);
                state.expired = None;
                Some(token)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    provider = self.provider.as_str(),
                    error = %format!("{e:#}"),
                    "credential fallback is not available"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    provider = self.provider.as_str(),
                    timeout_secs = self.probe_timeout.as_secs(),
                    "credential fallback did not answer in time"
                );
                None
            }
        }
    }
}

/// What health reports about one provider's credential. Names types and
/// sources only, never credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialReport {
    pub provider: String,
    /// The configured source.
    pub source: String,
    /// The credential actually in use.
    pub kind: String,
    /// Whether a fallback has taken over from the configured credential.
    pub fallback_active: bool,
    pub verdict: CredentialVerdict,
}

impl CredentialReport {
    /// The `credential` object in `/health/deep`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.verdict.status.as_str(),
            "reason": self.verdict.reason,
            "remediation": self.verdict.remediation,
            "source": self.source,
            "kind": self.kind,
            "fallback_active": self.fallback_active,
        })
    }
}

/// Which providers accept `credential_source`, the values each accepts, and
/// the provider's own validation of the rest of its credential settings.
/// Declared per implementation; a provider absent from this list rejects the
/// key at load time.
pub struct CredentialSupport {
    pub provider: &'static str,
    pub values: &'static [&'static str],
    pub validate: fn(&crate::config::schema::ProviderConfig) -> anyhow::Result<()>,
}

pub fn credential_support() -> &'static [CredentialSupport] {
    use crate::config::schema::{
        validate_azure_credential, validate_gcp_credential, AZURE_CREDENTIAL_SOURCES,
        GCP_CREDENTIAL_SOURCES,
    };
    const SUPPORT: &[CredentialSupport] = &[
        CredentialSupport {
            provider: "vertex",
            values: GCP_CREDENTIAL_SOURCES,
            validate: validate_gcp_credential,
        },
        CredentialSupport {
            provider: "azure",
            values: AZURE_CREDENTIAL_SOURCES,
            validate: validate_azure_credential,
        },
        CredentialSupport {
            provider: "foundry",
            values: AZURE_CREDENTIAL_SOURCES,
            validate: validate_azure_credential,
        },
        CredentialSupport {
            provider: "bing_grounding",
            values: AZURE_CREDENTIAL_SOURCES,
            validate: validate_azure_credential,
        },
    ];
    SUPPORT
}

pub fn support_for(provider: &str) -> Option<&'static CredentialSupport> {
    credential_support().iter().find(|s| s.provider == provider)
}

#[cfg(test)]
pub(crate) mod testing {
    //! A scripted [`CredentialSource`] for exercising the chain without any
    //! identity provider.
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub const REAUTH: &str = "REAUTH-REQUIRED: the login has lapsed";

    pub struct FakeSource {
        pub kind: String,
        pub result: Result<String, String>,
        pub calls: AtomicUsize,
        pub verdict: CredentialVerdict,
        pub fallback: Option<FallbackFactory>,
    }

    impl FakeSource {
        pub fn new(kind: &str, result: Result<&str, &str>) -> Self {
            Self {
                kind: kind.into(),
                result: result.map(str::to_string).map_err(str::to_string),
                calls: AtomicUsize::new(0),
                verdict: CredentialVerdict::new(
                    CredentialStatus::Warn,
                    "personal login",
                    "use the workload identity",
                ),
                fallback: None,
            }
        }
        pub fn with_fallback(mut self, f: FallbackFactory) -> Self {
            self.fallback = Some(f);
            self
        }
        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CredentialSource for FakeSource {
        async fn token(&self) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone().map_err(anyhow::Error::msg)
        }
        fn kind(&self) -> String {
            self.kind.clone()
        }
        fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
            format!("{err:#}").contains("REAUTH-REQUIRED")
        }
        fn verdict(&self) -> CredentialVerdict {
            self.verdict.clone()
        }
        fn expired_guidance(&self) -> (String, String) {
            (
                "the fake login expired".into(),
                "log in to the fake again".into(),
            )
        }
        fn fallback(&self) -> Option<FallbackFactory> {
            self.fallback.clone()
        }
    }

    pub fn factory(source: Arc<FakeSource>, builds: Arc<AtomicUsize>) -> FallbackFactory {
        Arc::new(move || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(source.clone() as Arc<dyn CredentialSource>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn chain(primary: FakeSource) -> (CredentialChain, Arc<FakeSource>) {
        let primary = Arc::new(primary);
        let c = CredentialChain::new("fakecloud", "default", primary.clone())
            .with_timings(Duration::from_secs(1), Duration::ZERO);
        (c, primary)
    }

    #[tokio::test]
    async fn a_healthy_source_reports_its_own_verdict() {
        let (c, _) = chain(FakeSource::new("fake-user", Ok("t")));
        assert_eq!(c.token().await.unwrap(), "t");
        let json = c.report().to_json();
        assert_eq!(json["status"], "warn");
        assert_eq!(json["reason"], "personal login");
        assert_eq!(json["remediation"], "use the workload identity");
        assert_eq!(json["source"], "default");
        assert_eq!(json["kind"], "fake-user");
        assert_eq!(json["fallback_active"], false);
    }

    #[tokio::test]
    async fn reauth_without_fallback_is_typed_credential_expired_and_reported_expired() {
        let (c, _) = chain(FakeSource::new("fake-user", Err(REAUTH)));
        let err = c.token().await.unwrap_err();
        let expired = find_credential_expired(&err).expect("typed permanent error");
        assert_eq!(expired.provider, "fakecloud");
        assert_eq!(expired.credential_kind, "fake-user");
        assert_eq!(expired.reason, "the fake login expired");
        assert_eq!(expired.remediation, "log in to the fake again");
        assert!(expired.detail.contains("REAUTH-REQUIRED"));

        let json = c.report().to_json();
        assert_eq!(json["status"], "expired");
        assert_eq!(json["reason"], "the fake login expired");
        assert_eq!(json["remediation"], "log in to the fake again");
    }

    #[tokio::test]
    async fn a_success_after_expiry_clears_the_expired_verdict() {
        struct Flaky(AtomicUsize);
        #[async_trait]
        impl CredentialSource for Flaky {
            async fn token(&self) -> anyhow::Result<String> {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("{REAUTH}")
                }
                Ok("fresh".into())
            }
            fn kind(&self) -> String {
                "fake-user".into()
            }
            fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
                err.to_string().contains("REAUTH-REQUIRED")
            }
            fn verdict(&self) -> CredentialVerdict {
                CredentialVerdict::ok("fine")
            }
            fn expired_guidance(&self) -> (String, String) {
                ("dead".into(), "fix".into())
            }
        }
        let c = CredentialChain::new("fakecloud", "x", Arc::new(Flaky(AtomicUsize::new(0))))
            .with_timings(Duration::from_secs(1), Duration::ZERO);
        c.token().await.unwrap_err();
        assert_eq!(c.report().verdict.status, CredentialStatus::Expired);
        assert_eq!(c.token().await.unwrap(), "fresh");
        assert_eq!(c.report().verdict.status, CredentialStatus::Ok);
    }

    #[tokio::test]
    async fn transient_errors_pass_through_untyped() {
        let builds = Arc::new(AtomicUsize::new(0));
        let fb = Arc::new(FakeSource::new("fake-workload", Ok("wl")));
        let (c, _) = chain(
            FakeSource::new("fake-user", Err("connection reset by peer"))
                .with_fallback(factory(fb, builds.clone())),
        );
        let err = c.token().await.unwrap_err();
        assert!(find_credential_expired(&err).is_none());
        assert_eq!(
            builds.load(Ordering::SeqCst),
            0,
            "a transient error must not trigger fallback"
        );
        assert_eq!(c.report().verdict.status, CredentialStatus::Warn);
    }

    #[tokio::test]
    async fn reauth_falls_back_and_stays_on_the_fallback() {
        let builds = Arc::new(AtomicUsize::new(0));
        let mut fb = FakeSource::new("fake-workload", Ok("wl-token"));
        fb.verdict = CredentialVerdict::ok("workload identity");
        let fb = Arc::new(fb);
        let (c, primary) = chain(
            FakeSource::new("fake-user", Err(REAUTH))
                .with_fallback(factory(fb.clone(), builds.clone())),
        );
        assert_eq!(c.token().await.unwrap(), "wl-token");
        assert_eq!(c.force_refresh().await.unwrap(), "wl-token");
        assert_eq!(
            primary.calls(),
            1,
            "primary is not asked again after the switch"
        );
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let json = c.report().to_json();
        assert_eq!(json["kind"], "fake-workload");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["fallback_active"], true);
    }

    #[tokio::test]
    async fn an_unreachable_fallback_leaves_credential_expired() {
        let builds = Arc::new(AtomicUsize::new(0));
        let fb = Arc::new(FakeSource::new("fake-workload", Err("no route to host")));
        let (c, _) =
            chain(FakeSource::new("fake-user", Err(REAUTH)).with_fallback(factory(fb, builds)));
        let err = c.token().await.unwrap_err();
        assert!(find_credential_expired(&err).is_some());
        assert!(!c.fallback_active());
    }

    #[tokio::test]
    async fn a_hanging_fallback_does_not_hang_the_request() {
        struct Hangs;
        #[async_trait]
        impl CredentialSource for Hangs {
            async fn token(&self) -> anyhow::Result<String> {
                std::future::pending().await
            }
            fn kind(&self) -> String {
                "hangs".into()
            }
            fn is_reauth_required(&self, _: &anyhow::Error) -> bool {
                false
            }
            fn verdict(&self) -> CredentialVerdict {
                CredentialVerdict::ok("")
            }
            fn expired_guidance(&self) -> (String, String) {
                (String::new(), String::new())
            }
        }
        let factory: FallbackFactory =
            Arc::new(|| Ok(Arc::new(Hangs) as Arc<dyn CredentialSource>));
        let c = CredentialChain::new(
            "fakecloud",
            "default",
            Arc::new(FakeSource::new("fake-user", Err(REAUTH)).with_fallback(factory)),
        )
        .with_timings(Duration::from_millis(20), Duration::ZERO);
        let err = c.token().await.unwrap_err();
        assert!(find_credential_expired(&err).is_some());
    }

    #[tokio::test]
    async fn a_dead_credential_is_answered_from_memory_within_the_recheck_interval() {
        let primary = Arc::new(FakeSource::new("fake-user", Err(REAUTH)));
        let c = CredentialChain::new("fakecloud", "default", primary.clone())
            .with_timings(Duration::from_secs(1), Duration::from_secs(3600));
        for _ in 0..5 {
            let err = c.token().await.unwrap_err();
            assert!(find_credential_expired(&err).is_some());
        }
        assert_eq!(
            primary.calls(),
            1,
            "a known-dead credential must not be refreshed per request"
        );
    }

    #[tokio::test]
    async fn fallback_probes_are_spaced_by_the_recheck_interval() {
        let builds = Arc::new(AtomicUsize::new(0));
        let fb = Arc::new(FakeSource::new("fake-workload", Err("absent")));
        let c = CredentialChain::new(
            "fakecloud",
            "default",
            Arc::new(
                FakeSource::new("fake-user", Err(REAUTH))
                    .with_fallback(factory(fb, builds.clone())),
            ),
        )
        .with_timings(Duration::from_secs(1), Duration::from_secs(3600));
        c.token().await.unwrap_err();
        c.force_refresh().await.unwrap_err();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_fallback_that_later_dies_is_itself_reported_expired() {
        let builds = Arc::new(AtomicUsize::new(0));
        struct DiesSecondTime(AtomicUsize);
        #[async_trait]
        impl CredentialSource for DiesSecondTime {
            async fn token(&self) -> anyhow::Result<String> {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok("wl".into());
                }
                anyhow::bail!("{REAUTH}")
            }
            fn kind(&self) -> String {
                "fake-workload".into()
            }
            fn is_reauth_required(&self, err: &anyhow::Error) -> bool {
                err.to_string().contains("REAUTH-REQUIRED")
            }
            fn verdict(&self) -> CredentialVerdict {
                CredentialVerdict::ok("workload")
            }
            fn expired_guidance(&self) -> (String, String) {
                (
                    "workload identity refused".into(),
                    "check its role assignment".into(),
                )
            }
        }
        let fb: Arc<dyn CredentialSource> = Arc::new(DiesSecondTime(AtomicUsize::new(0)));
        let b = builds.clone();
        let factory: FallbackFactory = Arc::new(move || {
            b.fetch_add(1, Ordering::SeqCst);
            Ok(fb.clone())
        });
        let (c, _) = chain(FakeSource::new("fake-user", Err(REAUTH)).with_fallback(factory));
        assert_eq!(c.token().await.unwrap(), "wl");
        let err = c.token().await.unwrap_err();
        let expired = find_credential_expired(&err).unwrap();
        assert_eq!(expired.credential_kind, "fake-workload");
        assert_eq!(c.report().to_json()["status"], "expired");
        assert_eq!(
            c.report().to_json()["remediation"],
            "check its role assignment"
        );
    }

    /// The breaker and the retry loop step aside for the error the chain
    /// produces, whichever provider's source raised it.
    #[tokio::test]
    async fn the_chain_error_is_exempt_from_breaker_and_retry() {
        use crate::router::retry::RetryableError;
        let (c, _) = chain(FakeSource::new("fake-user", Err(REAUTH)));
        let err = c.token().await.unwrap_err();
        let classified = RetryableError::classify_error(&err);
        assert!(matches!(classified, RetryableError::CredentialExpired));
        assert!(!classified.counts_toward_circuit_breaker());

        let breaker = crate::router::circuit_breaker::CircuitBreaker::new(1, 60);
        for _ in 0..5 {
            breaker.record_provider_failure("fakecloud", &err);
        }
        assert!(
            !breaker.is_open("fakecloud"),
            "credential_expired must never open the breaker"
        );
    }

    #[test]
    fn statuses_serialise_lowercase() {
        for (s, want) in [
            (CredentialStatus::Ok, "ok"),
            (CredentialStatus::Warn, "warn"),
            (CredentialStatus::Expired, "expired"),
            (CredentialStatus::Unknown, "unknown"),
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), want);
            assert_eq!(s.as_str(), want);
        }
    }

    #[test]
    fn support_is_declared_per_provider() {
        assert!(support_for("vertex").unwrap().values.contains(&"metadata"));
        assert!(support_for("foundry")
            .unwrap()
            .values
            .contains(&"managed-identity"));
        assert!(support_for("openai").is_none());
    }
}
