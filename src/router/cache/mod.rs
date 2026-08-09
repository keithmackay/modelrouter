//! Router-side response cache for LLM completions and web search.
//!
//! Every app behind the router gets the saving automatically: identical eligible
//! requests are served from a store at zero provider cost, metered as cache hits
//! so the saving is visible next to spend.
//!
//! Three deliberate design points:
//!
//! * **Key** — `sha256` over the resolved canonical model plus the full request
//!   body with only transport-level fields removed (see [`make_cache_key`]).
//!   Sampling parameters are part of the body, so changing `temperature`,
//!   `top_p`, `max_tokens`, `seed`, tools, or any other field yields a different
//!   key. An alias that re-resolves to a different model also yields a different
//!   key, because the resolved model is hashed in.
//! * **Eligibility** — conservative by default. Only requests whose
//!   `temperature` is explicitly at or below `cache.completions.max_temperature`
//!   (default `0.0`) are cached; a request that omits `temperature` is scored
//!   with `assumed_temperature` (default `1.0`) and is therefore *not* cached.
//!   Creative sampling never silently replays a stored answer.
//! * **Store** — behind the [`store::CacheStore`] trait, chosen by config. Ships
//!   `memory` (per-process) and `redis` (shared across stateless replicas).
//!
//! Exact-match only. Semantic/fuzzy matching is explicitly out of scope.

pub mod store;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::schema::{CacheConfig, CompletionCachePolicy, SearchCachePolicy};
use crate::providers::adapter::CompletionResult;
use store::{CacheStore, CachedEntry};

/// Request-body fields that describe *transport*, not the answer. Excluded from
/// the key so a streamed and a non-streamed ask for the same thing share an
/// entry, and so per-caller identifiers never fragment the cache.
const VOLATILE_FIELDS: &[&str] = &[
    "stream",
    "stream_options",
    "user",
    "session_id",
    "metadata",
];

/// Cache class, used as the first key segment and recorded on entries.
pub const CLASS_COMPLETION: &str = "completion";
pub const CLASS_SEARCH: &str = "search";

// ── Runtime policy ────────────────────────────────────────────────────────────

/// The live eligibility policy. Seeded from config, mutable at runtime through
/// the admin API/CLI (runtime changes are not written back to the config file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePolicy {
    pub enabled: bool,
    pub default_ttl_seconds: u64,
    pub completions: CompletionCachePolicy,
    pub search: SearchCachePolicy,
}

impl CachePolicy {
    pub fn from_config(config: &CacheConfig) -> Self {
        Self {
            enabled: config.enabled,
            default_ttl_seconds: config.ttl_seconds,
            completions: config.completions.clone(),
            search: config.search.clone(),
        }
    }

    pub fn completion_ttl(&self) -> Duration {
        Duration::from_secs(
            self.completions
                .ttl_seconds
                .unwrap_or(self.default_ttl_seconds),
        )
    }

    pub fn search_ttl(&self) -> Duration {
        Duration::from_secs(self.search.ttl_seconds)
    }
}

/// Fields an operator may change at runtime. All optional — omitted fields keep
/// their current value.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CachePolicyUpdate {
    pub enabled: Option<bool>,
    pub default_ttl_seconds: Option<u64>,
    pub completions_enabled: Option<bool>,
    pub completions_max_temperature: Option<f64>,
    pub completions_assumed_temperature: Option<f64>,
    pub completions_ttl_seconds: Option<u64>,
    pub search_enabled: Option<bool>,
    pub search_ttl_seconds: Option<u64>,
}

// ── Stats ─────────────────────────────────────────────────────────────────────

/// Live counters for this process. Cross-replica lifetime hit rate comes from
/// the cost ledger (`cache_hit` column), not from here.
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub backend: String,
    pub enabled: bool,
    pub healthy: bool,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub evictions: u64,
    pub entries: u64,
    pub hit_rate: f64,
    pub saved_usd: f64,
    pub by_model: Vec<ModelCacheStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCacheStats {
    pub model: String,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub saved_usd: f64,
}

#[derive(Default)]
struct ModelCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    saved_micro_usd: AtomicU64,
}

// ── The cache ─────────────────────────────────────────────────────────────────

/// Facade over a [`CacheStore`] that owns eligibility, key derivation, and
/// hit/miss accounting. Call sites use only this type.
pub struct ResponseCache {
    store: Arc<dyn CacheStore>,
    policy: ArcSwap<CachePolicy>,
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
    saved_micro_usd: AtomicU64,
    by_model: DashMap<String, ModelCounters>,
}

impl ResponseCache {
    pub fn new(config: &CacheConfig) -> Self {
        Self::with_store(store::build_store(config), CachePolicy::from_config(config))
    }

    /// Build a cache over an explicit store — used by tests and by any future
    /// backend wired outside `build_store`.
    pub fn with_store(store: Arc<dyn CacheStore>, policy: CachePolicy) -> Self {
        Self {
            store,
            policy: ArcSwap::from_pointee(policy),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            saved_micro_usd: AtomicU64::new(0),
            by_model: DashMap::new(),
        }
    }

    pub fn policy(&self) -> Arc<CachePolicy> {
        self.policy.load_full()
    }

    /// Apply a partial policy update, returning the new policy.
    pub fn update_policy(&self, update: &CachePolicyUpdate) -> Arc<CachePolicy> {
        let mut next = (**self.policy.load()).clone();
        if let Some(v) = update.enabled {
            next.enabled = v;
        }
        if let Some(v) = update.default_ttl_seconds {
            next.default_ttl_seconds = v;
        }
        if let Some(v) = update.completions_enabled {
            next.completions.enabled = v;
        }
        if let Some(v) = update.completions_max_temperature {
            next.completions.max_temperature = v;
        }
        if let Some(v) = update.completions_assumed_temperature {
            next.completions.assumed_temperature = v;
        }
        if let Some(v) = update.completions_ttl_seconds {
            next.completions.ttl_seconds = Some(v);
        }
        if let Some(v) = update.search_enabled {
            next.search.enabled = v;
        }
        if let Some(v) = update.search_ttl_seconds {
            next.search.ttl_seconds = v;
        }
        self.policy.store(Arc::new(next));
        self.policy.load_full()
    }

    // ── Eligibility ───────────────────────────────────────────────────────────

    /// Is this completion request deterministic enough to serve from cache?
    ///
    /// Streaming requests are never cached: the router would have to synthesize
    /// an SSE stream from a stored body, and callers asking for a stream are
    /// generally asking for a fresh generation.
    pub fn completion_eligible(&self, body: &Value) -> bool {
        let policy = self.policy.load();
        if !policy.enabled || !policy.completions.enabled {
            return false;
        }
        if body["stream"].as_bool().unwrap_or(false) {
            return false;
        }
        let temperature = body
            .get("temperature")
            .and_then(|v| v.as_f64())
            .unwrap_or(policy.completions.assumed_temperature);
        temperature <= policy.completions.max_temperature
    }

    pub fn search_eligible(&self) -> bool {
        let policy = self.policy.load();
        policy.enabled && policy.search.enabled
    }

    // ── Typed access ──────────────────────────────────────────────────────────

    /// Look up a completion. Records the hit/miss against `model`.
    pub async fn get_completion(&self, key: &str, model: &str) -> Option<CompletionResult> {
        let entry = self.lookup(key, model).await?;
        match serde_json::from_value::<CompletionResult>(entry.payload) {
            Ok(result) => Some(result),
            Err(e) => {
                tracing::warn!(error = %e, "cached completion payload did not deserialize");
                None
            }
        }
    }

    pub async fn put_completion(
        &self,
        key: &str,
        model: &str,
        result: &CompletionResult,
        original_cost_usd: f64,
    ) {
        let Ok(payload) = serde_json::to_value(result) else {
            return;
        };
        self.store_entry(key, CLASS_COMPLETION, model, payload, original_cost_usd)
            .await;
    }

    /// Look up a search response envelope. Records the hit/miss against
    /// `search/{engine}`.
    pub async fn get_search(&self, key: &str, model: &str) -> Option<Value> {
        Some(self.lookup(key, model).await?.payload)
    }

    pub async fn put_search(&self, key: &str, model: &str, payload: Value, original_cost_usd: f64) {
        self.store_entry(key, CLASS_SEARCH, model, payload, original_cost_usd)
            .await;
    }

    async fn lookup(&self, key: &str, model: &str) -> Option<CachedEntry> {
        let entry = self.store.get(key).await;
        let counters = self.by_model.entry(model.to_string()).or_default();
        match entry {
            Some(entry) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                counters.hits.fetch_add(1, Ordering::Relaxed);
                let micro = (entry.original_cost_usd.max(0.0) * 1_000_000.0) as u64;
                self.saved_micro_usd.fetch_add(micro, Ordering::Relaxed);
                counters.saved_micro_usd.fetch_add(micro, Ordering::Relaxed);
                Some(entry)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                counters.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    async fn store_entry(
        &self,
        key: &str,
        class: &str,
        model: &str,
        payload: Value,
        original_cost_usd: f64,
    ) {
        let ttl = match class {
            CLASS_SEARCH => self.policy.load().search_ttl(),
            _ => self.policy.load().completion_ttl(),
        };
        self.store
            .put(
                key,
                CachedEntry {
                    class: class.to_string(),
                    model: model.to_string(),
                    payload,
                    original_cost_usd,
                    stored_at: 0,
                    expires_at: 0,
                },
                ttl,
            )
            .await;
        self.stores.fetch_add(1, Ordering::Relaxed);
    }

    // ── Operator surface ──────────────────────────────────────────────────────

    pub async fn purge_all(&self) -> u64 {
        self.store.purge_all().await
    }

    pub async fn purge_model(&self, model: &str) -> u64 {
        self.store.purge_model(&model_fingerprint(model)).await
    }

    pub async fn purge_key(&self, key: &str) -> u64 {
        u64::from(self.store.purge_key(key).await)
    }

    pub async fn stats(&self) -> CacheStats {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let mut by_model: Vec<ModelCacheStats> = self
            .by_model
            .iter()
            .map(|e| {
                let h = e.value().hits.load(Ordering::Relaxed);
                let m = e.value().misses.load(Ordering::Relaxed);
                ModelCacheStats {
                    model: e.key().clone(),
                    hits: h,
                    misses: m,
                    hit_rate: hit_rate(h, m),
                    saved_usd: e.value().saved_micro_usd.load(Ordering::Relaxed) as f64
                        / 1_000_000.0,
                }
            })
            .collect();
        by_model.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.model.cmp(&b.model)));

        let policy = self.policy.load();
        CacheStats {
            backend: self.store.backend_name().to_string(),
            enabled: policy.enabled,
            healthy: self.store.healthy().await,
            hits,
            misses,
            stores: self.stores.load(Ordering::Relaxed),
            evictions: self.store.evictions(),
            entries: self.store.entry_count().await,
            hit_rate: hit_rate(hits, misses),
            saved_usd: self.saved_micro_usd.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            by_model,
        }
    }
}

pub fn hit_rate(hits: u64, misses: u64) -> f64 {
    let total = hits + misses;
    if total == 0 {
        0.0
    } else {
        hits as f64 / total as f64
    }
}

// ── Key derivation ────────────────────────────────────────────────────────────

/// Short, glob-safe fingerprint of a model name. Keys embed it so a backend can
/// purge by model with a pattern scan without parsing entries.
pub fn model_fingerprint(model: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Hex SHA-256 of the request body with transport-only fields removed.
///
/// `serde_json` maps are sorted, so field order in the incoming JSON does not
/// change the hash. Everything that can change the answer — messages, tools,
/// `temperature`, `top_p`, `max_tokens`, `seed`, `response_format`, … — is
/// hashed.
pub fn make_cache_key(body: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut canonical = body.clone();
    if let Some(obj) = canonical.as_object_mut() {
        for field in VOLATILE_FIELDS {
            obj.remove(*field);
        }
        // Router-internal annotations injected before the hook pipeline.
        obj.retain(|k, _| !k.starts_with("_mr_"));
    }
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(&canonical)
            .unwrap_or_default()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

/// Full store key for a completion: `completion:{model_fp}:{body_hash}`.
///
/// The resolved (post-alias, post-load-balancer) model is used, so two aliases
/// pointing at the same model share entries and a re-pointed alias naturally
/// misses instead of replaying another model's answer.
pub fn completion_cache_key(resolved_model: &str, body: &Value) -> String {
    format!(
        "{}:{}:{}",
        CLASS_COMPLETION,
        model_fingerprint(resolved_model),
        make_cache_key(body)
    )
}

/// Full store key for a search: `search:{engine_fp}:{hash(engine, query, options)}`.
pub fn search_cache_key(engine: &str, query: &str, max_results: Option<u32>) -> String {
    let canonical = serde_json::json!({
        "engine": engine,
        "query": query,
        "max_results": max_results,
    });
    format!(
        "{}:{}:{}",
        CLASS_SEARCH,
        model_fingerprint(&format!("search/{}", engine)),
        make_cache_key(&canonical)
    )
}
