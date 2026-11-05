//! Readiness that means "can serve".
//!
//! A replica may receive traffic only when it can complete a request: the
//! store answers, every provider adapter it may route to was built, and the
//! schema is at the version this binary expects. `serve` reports each step
//! into one shared [`Readiness`] handle as it completes, and `GET
//! /health/ready` reads the handle on every poll. Liveness stays separate and
//! never consults this.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::db::repositories::HealthRepository;

/// How long one store round trip answers for. Probes from several controllers
/// arrive within the same second; one query covers all of them.
const DB_PROBE_TTL: Duration = Duration::from_secs(1);

/// Shared, cheaply cloneable readiness state. Injected into the router as an
/// axum `Extension`, so the many hand-built `AppState` literals stay unchanged.
#[derive(Clone)]
pub struct Readiness(Arc<Inner>);

struct Inner {
    migrated: AtomicBool,
    draining: AtomicBool,
    provider_failures: RwLock<Vec<ProviderFailure>>,
    db_probe: tokio::sync::Mutex<Option<(Instant, Option<String>)>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFailure {
    pub provider: String,
    pub error: String,
}

/// One reason the replica cannot serve. Serialised into the 503 body so an
/// operator reading the probe response sees which part failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "part", rename_all = "snake_case")]
pub enum NotReady {
    Draining,
    Migrations,
    Database { detail: String },
    Provider { name: String, detail: String },
}

impl Readiness {
    /// Nothing proven yet: migrations pending, no provider results. `serve`
    /// marks each step as it completes.
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            migrated: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            provider_failures: RwLock::new(Vec::new()),
            db_probe: tokio::sync::Mutex::new(None),
        }))
    }

    /// A process whose startup steps already ran. For in-process tests and
    /// callers that build a router without going through `serve`.
    pub fn started() -> Self {
        let r = Self::new();
        r.mark_migrated();
        r
    }

    pub fn mark_migrated(&self) {
        self.0.migrated.store(true, Ordering::Release);
    }

    pub fn begin_drain(&self) {
        self.0.draining.store(true, Ordering::Release);
    }

    pub fn is_draining(&self) -> bool {
        self.0.draining.load(Ordering::Acquire)
    }

    /// Record the outcome of building each provider adapter. Replaces any
    /// earlier record, so a later warm pass clears earlier failures.
    pub fn record_provider_results(&self, results: Vec<(String, Result<(), String>)>) {
        let failures = results
            .into_iter()
            .filter_map(|(provider, result)| {
                result
                    .err()
                    .map(|error| ProviderFailure { provider, error })
            })
            .collect();
        *self
            .0
            .provider_failures
            .write()
            .unwrap_or_else(|e| e.into_inner()) = failures;
    }

    pub fn provider_failures(&self) -> Vec<ProviderFailure> {
        self.0
            .provider_failures
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Every reason the replica cannot serve right now; empty means ready.
    /// The store probe is the only I/O and is cached for [`DB_PROBE_TTL`].
    pub async fn failing<D: HealthRepository + ?Sized>(&self, db: &D) -> Vec<NotReady> {
        let mut failing = Vec::new();
        if self.is_draining() {
            failing.push(NotReady::Draining);
        }
        if !self.0.migrated.load(Ordering::Acquire) {
            failing.push(NotReady::Migrations);
        }
        for f in self.provider_failures() {
            failing.push(NotReady::Provider {
                name: f.provider,
                detail: f.error,
            });
        }
        if let Some(detail) = self.probe_db(db).await {
            failing.push(NotReady::Database { detail });
        }
        failing
    }

    async fn probe_db<D: HealthRepository + ?Sized>(&self, db: &D) -> Option<String> {
        let mut cached = self.0.db_probe.lock().await;
        if let Some((at, result)) = cached.as_ref() {
            if at.elapsed() < DB_PROBE_TTL {
                return result.clone();
            }
        }
        let result = db.ping().await.err().map(|e| e.to_string());
        *cached = Some((Instant::now(), result.clone()));
        result
    }
}

impl Default for Readiness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;

    struct CountingDb {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl HealthRepository for CountingDb {
        async fn ping(&self) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("connection refused")
            }
            Ok(())
        }
    }

    fn db(fail: bool) -> CountingDb {
        CountingDb {
            calls: AtomicUsize::new(0),
            fail,
        }
    }

    #[tokio::test]
    async fn fresh_handle_reports_migrations_pending_until_marked() {
        let r = Readiness::new();
        assert_eq!(r.failing(&db(false)).await, vec![NotReady::Migrations]);
        r.mark_migrated();
        assert!(r.failing(&db(false)).await.is_empty());
    }

    #[tokio::test]
    async fn started_handle_is_ready_against_a_healthy_store() {
        assert!(Readiness::started().failing(&db(false)).await.is_empty());
    }

    #[tokio::test]
    async fn store_failure_is_named_with_its_error() {
        let r = Readiness::started();
        assert_eq!(
            r.failing(&db(true)).await,
            vec![NotReady::Database {
                detail: "connection refused".to_string()
            }]
        );
    }

    #[tokio::test]
    async fn store_probe_is_cached_within_the_ttl() {
        let r = Readiness::started();
        let store = db(false);
        r.failing(&store).await;
        r.failing(&store).await;
        assert_eq!(store.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn provider_failures_are_listed_and_a_later_record_replaces_them() {
        let r = Readiness::started();
        r.record_provider_results(vec![
            ("openai".to_string(), Ok(())),
            ("azure".to_string(), Err("missing api_base".to_string())),
        ]);
        assert_eq!(
            r.failing(&db(false)).await,
            vec![NotReady::Provider {
                name: "azure".to_string(),
                detail: "missing api_base".to_string()
            }]
        );
        r.record_provider_results(vec![("azure".to_string(), Ok(()))]);
        assert!(r.failing(&db(false)).await.is_empty());
    }

    #[tokio::test]
    async fn draining_is_reported_first() {
        let r = Readiness::new();
        r.begin_drain();
        assert_eq!(
            r.failing(&db(false)).await,
            vec![NotReady::Draining, NotReady::Migrations]
        );
    }

    #[test]
    fn not_ready_serialises_with_a_part_tag() {
        let v = serde_json::to_value(NotReady::Provider {
            name: "azure".to_string(),
            detail: "x".to_string(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"part": "provider", "name": "azure", "detail": "x"})
        );
        assert_eq!(
            serde_json::to_value(NotReady::Draining).unwrap(),
            serde_json::json!({"part": "draining"})
        );
    }
}
