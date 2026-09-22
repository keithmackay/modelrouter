//! Auth module for the Vertex provider. Exposes a `TokenProvider` trait so
//! the adapter can be tested without touching real Google OAuth.
//!
//! Real auth is wrapped around `google-cloud-auth` 1.9's
//! `AccessTokenCredentials`, which caches and auto-refreshes *access* tokens.
//! It does NOT re-read the underlying credential material (e.g. the ADC
//! file's refresh token) after construction — see [`RebuildingProvider`] for
//! why that matters and what we do about it.

use anyhow::Context;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Fetches a Google Cloud OAuth2 Bearer access token.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> anyhow::Result<String>;

    /// Force a credential rebuild even though `token()` last succeeded.
    ///
    /// `token()`'s rebuild path only fires when fetching the token itself
    /// fails — but issue #2879's actual outage never did that:
    /// `access_token()` kept returning `Ok` for the whole 8.5 hours,
    /// because a user ADC grant's *refresh token* had expired underneath an
    /// access token `google-cloud-auth` still considered current. What
    /// failed was Vertex's own authorization check on the prediction call —
    /// a 401 on a token the provider believed was fine. A caller that sees
    /// exactly that (a downstream 401/UNAUTHENTICATED after a successful
    /// `token()`) has evidence `token()` doesn't, and needs a way to say "no,
    /// really, rebuild" — see [`RebuildingProvider::force_rebuild_token`].
    ///
    /// Default: no rebuild capability, just re-fetch via `token()` — correct
    /// for `StaticTokenProvider` and any other source with nothing to
    /// rebuild.
    async fn force_rebuild_token(&self) -> anyhow::Result<String> {
        self.token().await
    }
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

/// Minimum time between two credential-rebuild *attempts*. Without this, a
/// permanently-broken credential (e.g. a revoked service account) would
/// trigger a fresh rebuild — including a re-read of the credentials file and,
/// for service accounts, a JWT-signing pass — on every single request that
/// comes in while it's down, which under load is a self-inflicted request
/// storm on top of an already-broken provider.
const REBUILD_COOLDOWN: Duration = Duration::from_secs(60);

/// A credential object that can hand out a bearer token. Implemented for
/// `google_cloud_auth`'s `AccessTokenCredentials` in production, and for
/// fakes in tests — see [`RebuildingProvider`].
#[async_trait]
trait AccessTokenSource: Clone + Send + Sync + 'static {
    async fn fetch_token(&self) -> anyhow::Result<String>;
}

#[async_trait]
impl AccessTokenSource for google_cloud_auth::credentials::AccessTokenCredentials {
    async fn fetch_token(&self) -> anyhow::Result<String> {
        let access = self
            .access_token()
            .await
            .map_err(|e| anyhow::Error::msg(e.to_string()))
            .context("failed to fetch GCP access token")?;
        Ok(access.token)
    }
}

struct RebuildState<T> {
    current: T,
    /// Bumped on every successful rebuild, so a caller that read `current`
    /// before taking the write lock can tell whether someone else already
    /// fixed it while it was waiting.
    generation: u64,
    last_rebuild_attempt: Option<Instant>,
}

/// Wraps a credential type `T` with a rebuild-on-failure policy: `new()`
/// builds `T` once, same as before, but `token()` no longer treats that build
/// as gospel forever. When a fetch fails, it rebuilds `T` from scratch (which,
/// for the Vertex builders, re-reads the credentials file / re-resolves ADC
/// from the environment) and retries once, so a credential that was fixed on
/// disk after the process started takes effect on the next failing request
/// instead of requiring a restart.
///
/// This is deliberately generic over `T` rather than hard-wired to
/// `google_cloud_auth`'s type: the interesting behaviour here (rebuild once,
/// cooldown, don't stampede) has nothing to do with Google's OAuth wire
/// format, and testing it against the real crate would mean either hitting
/// real Google endpoints or fighting a type with no seam for failure
/// injection. A fake `AccessTokenSource` gives the tests a credential that
/// fails on demand without any of that.
struct RebuildingProvider<T: AccessTokenSource> {
    build: Box<dyn Fn() -> anyhow::Result<T> + Send + Sync>,
    state: RwLock<RebuildState<T>>,
}

impl<T: AccessTokenSource> RebuildingProvider<T> {
    fn new(build: impl Fn() -> anyhow::Result<T> + Send + Sync + 'static) -> anyhow::Result<Self> {
        let current = build()?;
        Ok(Self {
            build: Box::new(build),
            state: RwLock::new(RebuildState {
                current,
                generation: 0,
                last_rebuild_attempt: None,
            }),
        })
    }

    async fn token(&self) -> anyhow::Result<String> {
        let (snapshot, generation) = {
            let state = self.state.read().await;
            (state.current.clone(), state.generation)
        };

        match snapshot.fetch_token().await {
            Ok(token) => Ok(token),
            Err(first_err) => {
                tracing::warn!(
                    error = %first_err,
                    "vertex: cached credential was rejected, attempting to rebuild it"
                );
                self.recover(first_err, generation).await
            }
        }
    }

    /// Force a rebuild even though the cached credential's own `fetch_token`
    /// would report success — the caller has independent evidence (a 401
    /// from the downstream API) that it's wrong anyway. Goes through the
    /// same `recover` policy (cooldown, generation check, logging) as the
    /// `token()` failure path, just entered from a different trigger.
    async fn force_rebuild_token(&self) -> anyhow::Result<String> {
        let generation = self.state.read().await.generation;
        tracing::warn!(
            "vertex: downstream rejected the cached credential (401/UNAUTHENTICATED) even \
             though the token provider believed it was valid — forcing a rebuild"
        );
        self.recover(
            anyhow::anyhow!(
                "downstream rejected the cached credential with 401/UNAUTHENTICATED"
            ),
            generation,
        )
        .await
    }

    /// Called after a fetch on the current credential has already failed.
    /// Takes the write lock, rebuilds (subject to the cooldown), and retries
    /// the fetch once against whatever credential ends up current.
    async fn recover(&self, first_err: anyhow::Error, seen_generation: u64) -> anyhow::Result<String> {
        let mut state = self.state.write().await;

        // Someone else already rebuilt (and possibly retried) while we were
        // waiting for the write lock — use what they produced instead of
        // rebuilding again.
        if state.generation != seen_generation {
            let current = state.current.clone();
            drop(state);
            return current
                .fetch_token()
                .await
                .context("token fetch failed even against a concurrently rebuilt credential");
        }

        let now = Instant::now();
        if let Some(last_attempt) = state.last_rebuild_attempt {
            if now.duration_since(last_attempt) < REBUILD_COOLDOWN {
                tracing::warn!(
                    cooldown_secs = REBUILD_COOLDOWN.as_secs(),
                    "vertex: credential rebuild suppressed — still within cooldown from a prior \
                     failed rebuild attempt"
                );
                return Err(first_err);
            }
        }
        state.last_rebuild_attempt = Some(now);

        match (self.build)() {
            Ok(rebuilt) => {
                state.current = rebuilt.clone();
                state.generation += 1;
                drop(state);
                tracing::info!(
                    "vertex: credential rebuild succeeded, retrying the token fetch"
                );
                rebuilt
                    .fetch_token()
                    .await
                    .context("token fetch failed even after a successful credential rebuild")
            }
            Err(build_err) => {
                tracing::error!(error = %build_err, "vertex: credential rebuild failed");
                Err(first_err.context(format!("credential rebuild also failed: {build_err}")))
            }
        }
    }
}

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
/// Tokens are cached and auto-refreshed by `google-cloud-auth`, but the
/// underlying credential material (the ADC file's contents, a service
/// account's key) is only ever read when the credential is (re)built. If
/// `token()` fails, the provider rebuilds from that same source — re-reading
/// the file / re-resolving ADC — and retries once, so a credential refreshed
/// on disk after this process started takes effect without a restart. See
/// [`RebuildingProvider`] for the rebuild/cooldown policy.
pub struct GoogleCloudAuthProvider {
    inner: RebuildingProvider<google_cloud_auth::credentials::AccessTokenCredentials>,
}

impl GoogleCloudAuthProvider {
    pub fn new(credentials_path: Option<&str>) -> anyhow::Result<Self> {
        let credentials_path = credentials_path.map(str::to_string);
        let inner = RebuildingProvider::new(move || {
            Self::build_credentials(credentials_path.as_deref())
        })?;
        Ok(Self { inner })
    }

    fn build_credentials(
        credentials_path: Option<&str>,
    ) -> anyhow::Result<google_cloud_auth::credentials::AccessTokenCredentials> {
        match credentials_path {
            Some(path) => {
                let raw = std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read {path}"))?;
                let json: serde_json::Value = serde_json::from_str(&raw)
                    .with_context(|| format!("{path} is not valid JSON"))?;
                google_cloud_auth::credentials::service_account::Builder::new(json)
                    .with_access_specifier(
                        google_cloud_auth::credentials::service_account::AccessSpecifier::from_scopes(
                            [CLOUD_PLATFORM_SCOPE],
                        ),
                    )
                    .build_access_token_credentials()
                    .map_err(|e| anyhow::Error::msg(e.to_string()))
                    .context("failed to build service-account credentials")
            }
            None => google_cloud_auth::credentials::Builder::default()
                .with_scopes([CLOUD_PLATFORM_SCOPE])
                .build_access_token_credentials()
                .map_err(|e| anyhow::Error::msg(e.to_string()))
                .context("failed to build ADC credentials"),
        }
    }
}

#[async_trait]
impl TokenProvider for GoogleCloudAuthProvider {
    async fn token(&self) -> anyhow::Result<String> {
        self.inner.token().await
    }

    async fn force_rebuild_token(&self) -> anyhow::Result<String> {
        self.inner.force_rebuild_token().await
    }
}

/// Send a request built by `build`, and — if Vertex answers with 401 even
/// though `token_provider` hands out a token successfully — force a
/// credential rebuild and resend exactly once.
///
/// This is the path that actually fired in the 2026-09-22 outage (issue
/// #2879): `token()` kept returning `Ok` for 8.5 hours while Vertex
/// rejected every prediction call with `401 ACCESS_TOKEN_TYPE_UNSUPPORTED`,
/// because the credential material behind an already-cached access token had
/// gone bad. A rebuild triggered only by `token()` failing never fires in
/// that scenario — this is the second, independent trigger the fix needs
/// (see `force_rebuild_token` above).
///
/// `build` is called once per attempt (up to two) with the bearer token to
/// use, so it must be cheap and side-effect-free — it typically just clones a
/// URL and a JSON body into a `RequestBuilder`.
pub(crate) async fn send_with_401_retry<F>(
    token_provider: &Arc<dyn TokenProvider>,
    send_failed_context: &'static str,
    mut build: F,
) -> anyhow::Result<reqwest::Response>
where
    F: FnMut(String) -> reqwest::RequestBuilder,
{
    let token = token_provider.token().await?;
    let resp = build(token).send().await.context(send_failed_context)?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(resp);
    }

    let body = resp.text().await.unwrap_or_default();
    tracing::warn!(
        body = %body,
        "vertex: request rejected with 401 even though the cached credential looked valid — \
         forcing a credential rebuild and retrying once"
    );
    let fresh_token = token_provider
        .force_rebuild_token()
        .await
        .context("credential rebuild after a 401 from Vertex also failed")?;
    tracing::info!("vertex: retrying once with a rebuilt credential after a 401");
    build(fresh_token)
        .send()
        .await
        .with_context(|| format!("{send_failed_context} (retry after credential rebuild)"))
}

/// Test helper shared across `vertex::adapter`, `vertex::embed` and
/// `vertex::search`: a `TokenProvider` that hands out one token from
/// `token()` and a different one from `force_rebuild_token()`, counting
/// calls to the latter. Used to prove that a 401 from a mock Vertex server
/// actually drives a forced rebuild and a retry with the new token — not
/// just that the code compiles against the trait.
#[cfg(test)]
pub(crate) struct RecordingTokenProvider {
    stale_token: String,
    fresh_token: String,
    rebuild_calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl RecordingTokenProvider {
    pub(crate) fn new(stale_token: &str, fresh_token: &str) -> Self {
        Self {
            stale_token: stale_token.to_string(),
            fresh_token: fresh_token.to_string(),
            rebuild_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn rebuild_calls(&self) -> usize {
        self.rebuild_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[async_trait]
impl TokenProvider for RecordingTokenProvider {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.stale_token.clone())
    }

    async fn force_rebuild_token(&self) -> anyhow::Result<String> {
        self.rebuild_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.fresh_token.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake credential whose `fetch_token` behaviour depends on which build
    /// it came from — `generation` 0 (the credential `new()` produces) always
    /// fails; anything rebuilt after that always succeeds. Models "the
    /// credential we started with is stale; a rebuild reads fresh material".
    #[derive(Clone)]
    struct FirstBuildFailsCreds {
        generation: usize,
    }

    #[async_trait]
    impl AccessTokenSource for FirstBuildFailsCreds {
        async fn fetch_token(&self) -> anyhow::Result<String> {
            if self.generation == 0 {
                anyhow::bail!("stale credential (simulated)");
            }
            Ok(format!("token-gen-{}", self.generation))
        }
    }

    /// A fake credential that never works, however many times it's rebuilt —
    /// models a permanently revoked credential.
    #[derive(Clone)]
    struct AlwaysFailsCreds;

    #[async_trait]
    impl AccessTokenSource for AlwaysFailsCreds {
        async fn fetch_token(&self) -> anyhow::Result<String> {
            anyhow::bail!("permanently broken credential (simulated)")
        }
    }

    /// A fake credential whose `fetch_token` always succeeds, at every
    /// generation — this is what actually happened in the #2879 outage:
    /// `google-cloud-auth`'s `access_token()` returned `Ok` for the entire
    /// 8.5 hours because the user-ADC access token it had cached was still
    /// individually well-formed. `token()`'s failure-triggered rebuild can
    /// never fire against a source shaped like this; only
    /// `force_rebuild_token()` — driven by a downstream 401 — can.
    #[derive(Clone)]
    struct AlwaysSucceedsCreds {
        generation: usize,
    }

    #[async_trait]
    impl AccessTokenSource for AlwaysSucceedsCreds {
        async fn fetch_token(&self) -> anyhow::Result<String> {
            Ok(format!("token-gen-{}", self.generation))
        }
    }

    #[tokio::test]
    async fn token_recovers_by_rebuilding_after_first_failure() {
        let build_count = Arc::new(AtomicUsize::new(0));
        let counter = build_count.clone();
        let provider = RebuildingProvider::new(move || {
            let generation = counter.fetch_add(1, Ordering::SeqCst);
            Ok(FirstBuildFailsCreds { generation })
        })
        .expect("construction builds generation 0, which always succeeds to build (only fetch fails)");

        let token = provider
            .token()
            .await
            .expect("token() should rebuild the credential and recover");

        assert_eq!(token, "token-gen-1");
        assert_eq!(
            build_count.load(Ordering::SeqCst),
            2,
            "expected one build at construction and exactly one rebuild"
        );
    }

    #[tokio::test]
    async fn cooldown_suppresses_a_rebuild_storm() {
        let build_count = Arc::new(AtomicUsize::new(0));
        let counter = build_count.clone();
        let provider = RebuildingProvider::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(AlwaysFailsCreds)
        })
        .expect("AlwaysFailsCreds always builds fine; only fetch_token fails");

        for _ in 0..5 {
            let err = provider
                .token()
                .await
                .expect_err("credential is permanently broken, every call should fail");
            assert!(err.to_string().contains("broken") || err.to_string().contains("cooldown") ||
                    format!("{err:#}").contains("broken"));
        }

        // One build at construction, one rebuild attempt on the first
        // failure, then four more calls that should all be suppressed by the
        // cooldown rather than rebuilding again.
        assert_eq!(
            build_count.load(Ordering::SeqCst),
            2,
            "cooldown should have suppressed rebuilds after the first attempt"
        );
    }

    #[tokio::test]
    async fn healthy_credential_never_triggers_a_rebuild() {
        let build_count = Arc::new(AtomicUsize::new(0));
        let counter = build_count.clone();
        let provider = RebuildingProvider::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(FirstBuildFailsCreds { generation: 1 })
        })
        .unwrap();

        for _ in 0..3 {
            let token = provider.token().await.expect("credential is healthy");
            assert_eq!(token, "token-gen-1");
        }

        assert_eq!(
            build_count.load(Ordering::SeqCst),
            1,
            "a working credential should never be rebuilt"
        );
    }

    #[tokio::test]
    async fn force_rebuild_token_rebuilds_even_though_the_cached_credential_never_fails(
        /* issue #2879 SPEC UPDATE 2: token() alone never fires here */
    ) {
        let build_count = Arc::new(AtomicUsize::new(0));
        let counter = build_count.clone();
        let provider = RebuildingProvider::new(move || {
            let generation = counter.fetch_add(1, Ordering::SeqCst);
            Ok(AlwaysSucceedsCreds { generation })
        })
        .unwrap();

        // `token()` alone must never rebuild a credential that fetches fine —
        // this is exactly the shape of the real outage: access_token() kept
        // succeeding while Vertex rejected the token downstream.
        let t1 = provider.token().await.unwrap();
        assert_eq!(t1, "token-gen-0");
        assert_eq!(
            build_count.load(Ordering::SeqCst),
            1,
            "token() must not rebuild a credential whose own fetch never fails"
        );

        // A caller with independent evidence (a 401 from the downstream API)
        // forces a rebuild anyway, and gets a token from the new generation.
        let t2 = provider.force_rebuild_token().await.unwrap();
        assert_eq!(t2, "token-gen-1");
        assert_eq!(
            build_count.load(Ordering::SeqCst),
            2,
            "force_rebuild_token must rebuild regardless of whether fetch_token would have failed"
        );
    }

    #[tokio::test]
    async fn force_rebuild_token_is_also_subject_to_the_cooldown() {
        let build_count = Arc::new(AtomicUsize::new(0));
        let counter = build_count.clone();
        let provider = RebuildingProvider::new(move || {
            let generation = counter.fetch_add(1, Ordering::SeqCst);
            Ok(AlwaysSucceedsCreds { generation })
        })
        .unwrap();

        assert!(
            provider.force_rebuild_token().await.is_ok(),
            "the first forced rebuild should succeed"
        );
        for _ in 0..4 {
            assert!(
                provider.force_rebuild_token().await.is_err(),
                "further forced rebuilds within the cooldown window must be suppressed, \
                 not silently repeated"
            );
        }

        assert_eq!(
            build_count.load(Ordering::SeqCst),
            2,
            "one build at construction plus exactly one forced rebuild — the cooldown applies to \
             force_rebuild_token exactly as it does to token()'s own rebuild path"
        );
    }
}
