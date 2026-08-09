//! Pluggable cache store backends.
//!
//! Call sites never touch a backend directly — they go through
//! [`super::ResponseCache`], which owns an `Arc<dyn CacheStore>`. Adding a new
//! backend (Memcached, DynamoDB, …) means implementing this trait and adding one
//! arm to [`build_store`]; no route, admin handler, or CLI code changes.
//!
//! Deployments are stateless, so the default `memory` backend is explicitly a
//! per-process cache: correct, but each replica warms its own copy. `redis`
//! shares one cache across replicas.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::schema::CacheConfig;

/// One cached response, independent of which endpoint produced it.
///
/// `payload` is the endpoint-specific body (a serialized `CompletionResult` for
/// completions, the search response envelope for search). Keeping it as JSON is
/// what lets one store serve both endpoints — and any future one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEntry {
    /// `"completion"` or `"search"`.
    pub class: String,
    /// Resolved model (or `search/{engine}`) this entry belongs to.
    pub model: String,
    pub payload: serde_json::Value,
    /// Provider cost of the original call, in USD. Reported as the saving when
    /// this entry is served.
    #[serde(default)]
    pub original_cost_usd: f64,
    /// Unix seconds when the entry was stored.
    #[serde(default)]
    pub stored_at: i64,
    /// Unix seconds after which the entry is stale. Redis enforces this itself;
    /// the memory backend checks it on read so both honour the same TTL.
    #[serde(default)]
    pub expires_at: i64,
}

/// A cache backend. Implementations must be safe to share across tasks.
#[async_trait]
pub trait CacheStore: Send + Sync {
    async fn get(&self, key: &str) -> Option<CachedEntry>;
    async fn put(&self, key: &str, entry: CachedEntry, ttl: Duration);
    /// Remove one key. Returns true if it existed.
    async fn purge_key(&self, key: &str) -> bool;
    /// Remove every entry whose key carries `model_fp` (see
    /// [`super::model_fingerprint`]). Returns the number removed.
    async fn purge_model(&self, model_fp: &str) -> u64;
    /// Remove everything in this namespace. Returns the number removed.
    async fn purge_all(&self) -> u64;
    /// Approximate live entry count.
    async fn entry_count(&self) -> u64;
    /// Entries dropped by capacity/TTL eviction since process start.
    /// Redis evicts on its own schedule and does not report this: returns 0.
    fn evictions(&self) -> u64 {
        0
    }
    fn backend_name(&self) -> &'static str;
    /// False when the backend is unreachable (e.g. Redis is down). The router
    /// degrades to "always miss" rather than failing requests.
    async fn healthy(&self) -> bool {
        true
    }
}

/// Construct the configured backend. Falls back to memory (with a loud warning)
/// when `backend = "redis"` but no URL is configured, so a misconfiguration
/// costs money rather than availability.
pub fn build_store(config: &CacheConfig) -> Arc<dyn CacheStore> {
    match config.backend.as_str() {
        "redis" => {
            if config.redis_url.trim().is_empty() {
                tracing::error!(
                    "cache.backend = \"redis\" but cache.redis_url is empty; \
                     falling back to the in-memory store"
                );
                return Arc::new(MemoryStore::new(config));
            }
            match RedisStore::new(&config.redis_url, &config.namespace) {
                Ok(store) => Arc::new(store),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to open Redis cache client; falling back to the in-memory store"
                    );
                    Arc::new(MemoryStore::new(config))
                }
            }
        }
        "memory" => Arc::new(MemoryStore::new(config)),
        other => {
            tracing::warn!(
                backend = other,
                "unknown cache.backend; using the in-memory store"
            );
            Arc::new(MemoryStore::new(config))
        }
    }
}

// ── In-memory backend ─────────────────────────────────────────────────────────

/// Per-process moka cache. Bounded by `cache.max_entries`, per-entry TTL.
pub struct MemoryStore {
    inner: moka::future::Cache<String, CachedEntry>,
    evictions: Arc<AtomicU64>,
}

impl MemoryStore {
    pub fn new(config: &CacheConfig) -> Self {
        let evictions = Arc::new(AtomicU64::new(0));
        let ev = evictions.clone();
        let inner = moka::future::Cache::builder()
            .max_capacity(config.max_entries)
            .eviction_listener(move |_k, _v, cause| {
                // `Replaced` and explicit invalidation are not evictions.
                if matches!(
                    cause,
                    moka::notification::RemovalCause::Size
                        | moka::notification::RemovalCause::Expired
                ) {
                    ev.fetch_add(1, Ordering::Relaxed);
                }
            })
            .build();
        Self { inner, evictions }
    }
}

#[async_trait]
impl CacheStore for MemoryStore {
    async fn get(&self, key: &str) -> Option<CachedEntry> {
        let entry = self.inner.get(key).await?;
        if entry.expires_at > 0 && entry.expires_at <= now_secs() {
            // Expired but not yet evicted: drop it and report a miss.
            self.inner.invalidate(key).await;
            self.evictions.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(entry)
    }

    async fn put(&self, key: &str, entry: CachedEntry, ttl: Duration) {
        // Per-entry TTL lives on the entry rather than in a moka expiry policy,
        // so every backend enforces the same deadline the same way.
        let mut entry = entry;
        entry.stored_at = now_secs();
        entry.expires_at = entry.stored_at + ttl.as_secs().max(1) as i64;
        self.inner.insert(key.to_string(), entry).await;
    }

    async fn purge_key(&self, key: &str) -> bool {
        let existed = self.inner.get(key).await.is_some();
        self.inner.invalidate(key).await;
        existed
    }

    async fn purge_model(&self, model_fp: &str) -> u64 {
        // moka applies writes asynchronously; without this the iterator can miss
        // entries that were just inserted.
        self.inner.run_pending_tasks().await;
        let mut removed = 0;
        let keys: Vec<String> = self
            .inner
            .iter()
            .filter(|(k, _)| k.contains(model_fp))
            .map(|(k, _)| (*k).clone())
            .collect();
        for k in keys {
            self.inner.invalidate(&k).await;
            removed += 1;
        }
        removed
    }

    async fn purge_all(&self) -> u64 {
        self.inner.run_pending_tasks().await;
        let n = self.inner.entry_count();
        self.inner.invalidate_all();
        self.inner.run_pending_tasks().await;
        n
    }

    async fn entry_count(&self) -> u64 {
        self.inner.run_pending_tasks().await;
        self.inner.entry_count()
    }

    fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

// ── Redis backend ─────────────────────────────────────────────────────────────

/// Shared cache for stateless replicas. Entries are JSON values with a Redis
/// TTL, so expiry is enforced by the server rather than by any one process.
pub struct RedisStore {
    client: redis::Client,
    namespace: String,
}

impl RedisStore {
    pub fn new(url: &str, namespace: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        Ok(Self {
            client,
            namespace: namespace.to_string(),
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}:{}", self.namespace, key)
    }

    async fn conn(&self) -> Option<redis::aio::MultiplexedConnection> {
        match self.client.get_multiplexed_async_connection().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "redis cache unavailable; treating as miss");
                None
            }
        }
    }

    /// Collect keys matching a glob using SCAN (never KEYS — this runs against
    /// production Redis instances).
    async fn scan(&self, pattern: &str) -> Vec<String> {
        let Some(mut conn) = self.conn().await else {
            return Vec::new();
        };
        let mut cursor: u64 = 0;
        let mut found = Vec::new();
        loop {
            let res: redis::RedisResult<(u64, Vec<String>)> = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut conn)
                .await;
            match res {
                Ok((next, keys)) => {
                    found.extend(keys);
                    if next == 0 {
                        break;
                    }
                    cursor = next;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "redis SCAN failed");
                    break;
                }
            }
        }
        found
    }

    async fn delete_keys(&self, keys: Vec<String>) -> u64 {
        if keys.is_empty() {
            return 0;
        }
        let Some(mut conn) = self.conn().await else {
            return 0;
        };
        let mut removed = 0u64;
        for chunk in keys.chunks(200) {
            let mut cmd = redis::cmd("DEL");
            for k in chunk {
                cmd.arg(k);
            }
            match cmd.query_async::<u64>(&mut conn).await {
                Ok(n) => removed += n,
                Err(e) => tracing::warn!(error = %e, "redis DEL failed"),
            }
        }
        removed
    }
}

#[async_trait]
impl CacheStore for RedisStore {
    async fn get(&self, key: &str) -> Option<CachedEntry> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = redis::cmd("GET")
            .arg(self.full_key(key))
            .query_async(&mut conn)
            .await
            .ok()?;
        let raw = raw?;
        match serde_json::from_str::<CachedEntry>(&raw) {
            Ok(entry) => Some(entry),
            Err(e) => {
                tracing::warn!(error = %e, "discarding unparseable cache entry");
                None
            }
        }
    }

    async fn put(&self, key: &str, entry: CachedEntry, ttl: Duration) {
        let Some(mut conn) = self.conn().await else {
            return;
        };
        let ttl = ttl.as_secs().max(1);
        let mut entry = entry;
        entry.stored_at = now_secs();
        entry.expires_at = entry.stored_at + ttl as i64;
        let Ok(raw) = serde_json::to_string(&entry) else {
            return;
        };
        if let Err(e) = redis::cmd("SET")
            .arg(self.full_key(key))
            .arg(raw)
            .arg("EX")
            .arg(ttl)
            .query_async::<()>(&mut conn)
            .await
        {
            tracing::warn!(error = %e, "redis SET failed; response not cached");
        }
    }

    async fn purge_key(&self, key: &str) -> bool {
        self.delete_keys(vec![self.full_key(key)]).await > 0
    }

    async fn purge_model(&self, model_fp: &str) -> u64 {
        let keys = self
            .scan(&format!("{}:*:{}:*", self.namespace, model_fp))
            .await;
        self.delete_keys(keys).await
    }

    async fn purge_all(&self) -> u64 {
        let keys = self.scan(&format!("{}:*", self.namespace)).await;
        self.delete_keys(keys).await
    }

    async fn entry_count(&self) -> u64 {
        self.scan(&format!("{}:*", self.namespace)).await.len() as u64
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }

    async fn healthy(&self) -> bool {
        let Some(mut conn) = self.conn().await else {
            return false;
        };
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .is_ok()
    }
}

pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
