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

pub mod request;
pub mod store;
pub mod stream;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::schema::{
    CacheConfig, CompletionCachePolicy, NamespaceCacheConfig, SearchCachePolicy,
};
use crate::providers::adapter::CompletionResult;
pub use request::{CacheDirectives, CacheMode, CacheNamespace, CachePlan};
use store::{CacheStore, CachedEntry, EntryTtl};

/// Request-body fields that describe *transport*, not the answer. Excluded from
/// the key so a streamed and a non-streamed ask for the same thing share an
/// entry, and so per-caller identifiers never fragment the cache.
pub const VOLATILE_FIELDS: &[&str] = &[
    "stream",
    "stream_options",
    "user",
    "session_id",
    "metadata",
    // Caller-supplied cost attribution: metadata about *whose* work this is,
    // never about what the answer should be. Excluding it here is what stops
    // per-engagement tagging from fragmenting the cache into one entry per tag.
    "attribution",
];

/// Cache class, used as the first key segment and recorded on entries.
pub const CLASS_COMPLETION: &str = "completion";
pub const CLASS_SEARCH: &str = "search";
/// Anthropic Messages API responses, stored as the native `message` object.
/// Its own class because the payload shape differs from [`CLASS_COMPLETION`];
/// it shares the completion eligibility rules and TTL.
pub const CLASS_MESSAGES: &str = "messages";

// ── Runtime policy ────────────────────────────────────────────────────────────

/// The live eligibility policy. Seeded from config, mutable at runtime through
/// the admin API/CLI (runtime changes are not written back to the config file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePolicy {
    pub enabled: bool,
    pub default_ttl_seconds: u64,
    pub completions: CompletionCachePolicy,
    pub search: SearchCachePolicy,
    /// See [`CacheConfig::allow_header_opt_in`]. Config-only.
    pub allow_header_opt_in: bool,
    /// See [`CacheConfig::require_opt_in`]. Config-only.
    #[serde(default)]
    pub require_opt_in: bool,
    /// See [`CacheConfig::max_ttl_seconds`]. Config-only.
    pub max_ttl_seconds: u64,
    /// See [`CacheConfig::namespaces`]. Config-only.
    pub namespaces: std::collections::HashMap<String, NamespaceCacheConfig>,
}

impl CachePolicy {
    pub fn from_config(config: &CacheConfig) -> Self {
        Self {
            enabled: config.enabled,
            default_ttl_seconds: config.ttl_seconds,
            completions: config.completions.clone(),
            search: config.search.clone(),
            allow_header_opt_in: config.allow_header_opt_in,
            require_opt_in: config.require_opt_in,
            max_ttl_seconds: config.max_ttl_seconds,
            namespaces: config.namespaces.clone(),
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

    /// The lifetime of an entry stored in `class`: the caller's requested
    /// TTL capped by `max_ttl_seconds`, else the namespace's configured
    /// default, else the class default.
    pub fn entry_ttl(&self, class: &str, directives: &CacheDirectives) -> EntryTtl {
        if let Some(requested) = directives.ttl {
            return requested.min(EntryTtl::from_secs(self.max_ttl_seconds));
        }
        let namespace_ttl = directives
            .namespace
            .as_ref()
            .and_then(|ns| self.namespaces.get(ns.as_str()))
            .and_then(|config| config.ttl_seconds);
        if let Some(secs) = namespace_ttl {
            return EntryTtl::from_secs(secs);
        }
        EntryTtl::Finite(match class {
            CLASS_SEARCH => self.search_ttl(),
            _ => self.completion_ttl(),
        })
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
    /// `cache.require_opt_in`: only requests carrying `use`/`refresh` are cached.
    pub require_opt_in: bool,
    /// Requests skipped under `require_opt_in` because they carried no
    /// `x-modelrouter-cache` header. A rising count names a caller that is
    /// not opting in.
    pub skipped_without_opt_in: u64,
    pub by_model: Vec<ModelCacheStats>,
    /// Traffic that carried `x-modelrouter-cache-namespace`, per namespace.
    /// Requests without one appear only in the totals.
    pub by_namespace: Vec<NamespaceCacheStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCacheStats {
    pub model: String,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub saved_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NamespaceCacheStats {
    /// The namespace, or [`OTHER_NAMESPACES`] for traffic past the tracking
    /// limit.
    pub namespace: String,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub hit_rate: f64,
    pub saved_usd: f64,
}

/// Namespaces are caller-chosen, so per-namespace counters are bounded: past
/// this many, further namespaces are counted together under
/// [`OTHER_NAMESPACES`].
const MAX_TRACKED_NAMESPACES: usize = 256;

/// Stats bucket for namespaces past [`MAX_TRACKED_NAMESPACES`]. `*` is outside
/// the namespace charset, so it cannot collide with a real namespace.
pub const OTHER_NAMESPACES: &str = "*";

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
    saved_micro_usd: AtomicU64,
}

impl Counters {
    fn hit(&self, micro_usd: u64) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.saved_micro_usd.fetch_add(micro_usd, Ordering::Relaxed);
    }

    fn miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    fn saved_usd(&self) -> f64 {
        self.saved_micro_usd.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }
}

// ── The cache ─────────────────────────────────────────────────────────────────

/// Facade over a [`CacheStore`] that owns eligibility, key derivation, and
/// hit/miss accounting. Call sites use only this type.
pub struct ResponseCache {
    store: Arc<dyn CacheStore>,
    policy: ArcSwap<CachePolicy>,
    /// Configured Redis key prefix (`cache.namespace`), surfaced by /health so
    /// an operator can see at a glance WHICH cache a gateway is (or is not)
    /// attached to. Unrelated to the per-request [`CacheNamespace`].
    namespace: String,
    hits: AtomicU64,
    misses: AtomicU64,
    stores: AtomicU64,
    saved_micro_usd: AtomicU64,
    skipped_without_opt_in: AtomicU64,
    by_model: DashMap<String, Counters>,
    by_namespace: DashMap<String, Counters>,
}

impl ResponseCache {
    pub fn new(config: &CacheConfig) -> Self {
        let mut cache =
            Self::with_store(store::build_store(config), CachePolicy::from_config(config));
        cache.namespace = config.namespace.clone();
        cache
    }

    /// Build a cache over an explicit store — used by tests and by any future
    /// backend wired outside `build_store`.
    pub fn with_store(store: Arc<dyn CacheStore>, policy: CachePolicy) -> Self {
        Self {
            store,
            policy: ArcSwap::from_pointee(policy),
            namespace: String::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            saved_micro_usd: AtomicU64::new(0),
            skipped_without_opt_in: AtomicU64::new(0),
            by_model: DashMap::new(),
            by_namespace: DashMap::new(),
        }
    }

    pub fn policy(&self) -> Arc<CachePolicy> {
        self.policy.load_full()
    }

    // ── Health surfacing ──────────────────────────────────────────────────────

    /// Runtime-effective enabled flag (config seed + any admin override).
    pub fn enabled(&self) -> bool {
        self.policy.load().enabled
    }

    pub fn backend_name(&self) -> &'static str {
        self.store.backend_name()
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Live reachability of the backing store. For Redis this is a real PING
    /// (which also heals a dropped connection); for memory it is always true.
    pub async fn connected(&self) -> bool {
        self.store.healthy().await
    }

    pub async fn entry_count(&self) -> u64 {
        self.store.entry_count().await
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
    /// `stream` plays no part: a streamed and a plain request share an entry
    /// and a hit is replayed in whichever shape was asked for.
    pub fn completion_eligible(&self, body: &Value) -> bool {
        let policy = self.policy.load();
        if !policy.enabled || !policy.completions.enabled {
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

    /// The plan for a completion-shaped request (`/v1/chat/completions`,
    /// `/v1/messages`) under the caller's `mode`.
    pub fn completion_plan(&self, mode: CacheMode, body: &Value) -> CachePlan {
        let policy = self.policy.load();
        let enabled = policy.enabled && policy.completions.enabled;
        let eligible = self.completion_eligible(body);
        self.plan(&policy, mode, enabled, eligible)
    }

    /// The plan for a search under the caller's `mode`.
    pub fn search_plan(&self, mode: CacheMode) -> CachePlan {
        let policy = self.policy.load();
        let enabled = policy.enabled && policy.search.enabled;
        self.plan(&policy, mode, enabled, enabled)
    }

    /// Under `require_opt_in` nothing is eligible by default: only `use` and
    /// `refresh` reach the cache.
    fn plan(&self, policy: &CachePolicy, mode: CacheMode, enabled: bool, eligible: bool) -> CachePlan {
        if policy.require_opt_in && enabled && mode == CacheMode::Default {
            self.skipped_without_opt_in.fetch_add(1, Ordering::Relaxed);
        }
        let eligible = eligible && !policy.require_opt_in;
        CachePlan::decide(mode, enabled, eligible, policy.allow_header_opt_in)
    }

    // ── Typed access ──────────────────────────────────────────────────────────

    /// Look up a completion. Records the hit/miss against `model` and the
    /// caller's namespace.
    pub async fn get_completion(
        &self,
        key: &str,
        model: &str,
        directives: &CacheDirectives,
    ) -> Option<CompletionResult> {
        let entry = self.lookup(key, model, directives).await?;
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
        directives: &CacheDirectives,
    ) {
        let Ok(payload) = serde_json::to_value(result) else {
            return;
        };
        self.store_entry(
            key,
            CLASS_COMPLETION,
            model,
            payload,
            original_cost_usd,
            directives,
        )
        .await;
    }

    /// Look up a native Anthropic `message`. Records the hit/miss against
    /// `model`.
    pub async fn get_message(
        &self,
        key: &str,
        model: &str,
        directives: &CacheDirectives,
    ) -> Option<Value> {
        Some(self.lookup(key, model, directives).await?.payload)
    }

    pub async fn put_message(
        &self,
        key: &str,
        model: &str,
        message: Value,
        original_cost_usd: f64,
        directives: &CacheDirectives,
    ) {
        self.store_entry(
            key,
            CLASS_MESSAGES,
            model,
            message,
            original_cost_usd,
            directives,
        )
        .await;
    }

    /// Look up a search response envelope. Records the hit/miss against
    /// `search/{engine}`.
    pub async fn get_search(
        &self,
        key: &str,
        model: &str,
        directives: &CacheDirectives,
    ) -> Option<Value> {
        Some(self.lookup(key, model, directives).await?.payload)
    }

    pub async fn put_search(
        &self,
        key: &str,
        model: &str,
        payload: Value,
        original_cost_usd: f64,
        directives: &CacheDirectives,
    ) {
        self.store_entry(
            key,
            CLASS_SEARCH,
            model,
            payload,
            original_cost_usd,
            directives,
        )
        .await;
    }

    /// The namespace never takes part in the lookup: any caller asking the
    /// same question hits the same entry. It only picks the stats bucket.
    async fn lookup(
        &self,
        key: &str,
        model: &str,
        directives: &CacheDirectives,
    ) -> Option<CachedEntry> {
        let entry = self.store.get(key).await;
        let counters = self.by_model.entry(model.to_string()).or_default();
        let ns_counters = self.namespace_counters(directives.namespace.as_ref());
        match entry {
            Some(entry) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                let micro = (entry.original_cost_usd.max(0.0) * 1_000_000.0) as u64;
                self.saved_micro_usd.fetch_add(micro, Ordering::Relaxed);
                counters.hit(micro);
                if let Some(ns) = ns_counters {
                    ns.hit(micro);
                }
                Some(entry)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                counters.miss();
                if let Some(ns) = ns_counters {
                    ns.miss();
                }
                None
            }
        }
    }

    /// The counters for `namespace`; `None` for the default namespace.
    fn namespace_counters(
        &self,
        namespace: Option<&CacheNamespace>,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, Counters>> {
        let ns = namespace?.as_str();
        if let Some(counters) = self.by_namespace.get(ns) {
            return Some(counters);
        }
        let bucket = if self.by_namespace.len() < MAX_TRACKED_NAMESPACES {
            ns
        } else {
            OTHER_NAMESPACES
        };
        self.by_namespace.entry(bucket.to_string()).or_default();
        self.by_namespace.get(bucket)
    }

    async fn store_entry(
        &self,
        key: &str,
        class: &str,
        model: &str,
        payload: Value,
        original_cost_usd: f64,
        directives: &CacheDirectives,
    ) {
        let ttl = self.policy.load().entry_ttl(class, directives);
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
                    namespace: directives.namespace.as_ref().map(|ns| ns.to_string()),
                },
                ttl,
            )
            .await;
        self.stores.fetch_add(1, Ordering::Relaxed);
        if let Some(ns) = self.namespace_counters(directives.namespace.as_ref()) {
            ns.stores.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ── Operator surface ──────────────────────────────────────────────────────

    pub async fn purge_all(&self) -> u64 {
        self.store.purge_all().await
    }

    pub async fn purge_model(&self, model: &str) -> u64 {
        self.store.purge_model(&model_fingerprint(model)).await
    }

    /// Remove every entry that was stored by a request in `namespace`, across
    /// all classes and models. The default namespace cannot be purged on its
    /// own.
    pub async fn purge_namespace(&self, namespace: &CacheNamespace) -> u64 {
        self.store.purge_namespace(namespace.as_str()).await
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
                    saved_usd: e.value().saved_usd(),
                }
            })
            .collect();
        by_model.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.model.cmp(&b.model)));
        let mut by_namespace: Vec<NamespaceCacheStats> = self
            .by_namespace
            .iter()
            .map(|e| {
                let h = e.value().hits.load(Ordering::Relaxed);
                let m = e.value().misses.load(Ordering::Relaxed);
                NamespaceCacheStats {
                    namespace: e.key().clone(),
                    hits: h,
                    misses: m,
                    stores: e.value().stores.load(Ordering::Relaxed),
                    hit_rate: hit_rate(h, m),
                    saved_usd: e.value().saved_usd(),
                }
            })
            .collect();
        by_namespace.sort_by(|a, b| a.namespace.cmp(&b.namespace));

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
            require_opt_in: policy.require_opt_in,
            skipped_without_opt_in: self.skipped_without_opt_in.load(Ordering::Relaxed),
            by_model,
            by_namespace,
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

/// Full store key for a `/v1/messages` call: `messages:{model_fp}:{body_hash}`.
pub fn messages_cache_key(resolved_model: &str, body: &Value) -> String {
    format!(
        "{}:{}:{}",
        CLASS_MESSAGES,
        model_fingerprint(resolved_model),
        make_cache_key(body)
    )
}

/// Full store key for a search: `search:{engine_fp}:{hash(engine, query, options)}`.
/// The parts of a search request, beyond engine, query and count, that change
/// the payload: what shapes a generated answer, the age filter, and the
/// engine's result format. Each is part of the key.
#[derive(Debug, Default, Clone, Copy)]
pub struct SearchAnswerKey<'a> {
    pub include_answer: bool,
    pub instructions: Option<&'a str>,
    pub context: Option<&'a str>,
    pub max_follow_up_queries: u32,
    pub freshness: Option<&'a str>,
    /// `search_registry::result_format` for the engine: a change to how the
    /// engine builds its results retires what was cached before it.
    pub result_format: Option<&'a str>,
}

pub fn search_cache_key(
    engine: &str,
    query: &str,
    max_results: Option<u32>,
    answer: &SearchAnswerKey<'_>,
) -> String {
    let mut canonical = serde_json::json!({
        "engine": engine,
        "query": query,
        "max_results": max_results,
    });
    // Only keyed when set, so keys for plain requests are unchanged.
    if answer.include_answer {
        canonical["answer"] = serde_json::json!({
            "instructions": answer.instructions,
            "context": answer.context,
            "max_follow_up_queries": answer.max_follow_up_queries,
        });
    }
    if let Some(freshness) = answer.freshness {
        canonical["freshness"] = serde_json::json!(freshness);
    }
    if let Some(format) = answer.result_format {
        canonical["result_format"] = serde_json::json!(format);
    }
    format!(
        "{}:{}:{}",
        CLASS_SEARCH,
        model_fingerprint(&format!("search/{}", engine)),
        make_cache_key(&canonical)
    )
}
