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
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

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

/// How long to wait after a failed connect before dialling Redis again. While
/// the cooldown is armed every cache operation is an immediate miss, so a dead
/// Redis costs nothing per request instead of a connect timeout per request.
const REDIS_RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// Where the store believes the Redis link is. Only used to decide which
/// TRANSITIONS deserve a log line — per-operation noise stays at debug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkState {
    /// No connection has ever been established (process just started).
    NeverConnected,
    Reachable,
    Unreachable,
}

/// A state change worth telling the operator about, exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkTransition {
    /// No change — stay quiet (or debug-level at most).
    None,
    /// First successful connection since process start.
    Connected,
    /// Reachable (or never yet connected) -> unreachable.
    Lost,
    /// Unreachable -> reachable again.
    Recovered,
}

/// Pure reconnect/backoff decision logic, kept separate from any I/O so it can
/// be unit-tested without a Redis server.
///
/// Why this exists: the original store dialled Redis once per operation and
/// logged an identical warning every time, so an outage produced an unbounded
/// warn stream, a per-request connect attempt, and — because nothing tracked
/// state — no way to tell "still down" from "came back" without reading the
/// whole log (see issue #21).
#[derive(Debug)]
pub(crate) struct ReconnectGate {
    cooldown: Duration,
    state: LinkState,
    /// Armed after a failed connect; no new dial is attempted before it.
    retry_after: Option<Instant>,
}

impl ReconnectGate {
    pub(crate) fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            state: LinkState::NeverConnected,
            retry_after: None,
        }
    }

    /// May we dial now, or are we inside the post-failure cooldown window?
    pub(crate) fn should_attempt(&self, now: Instant) -> bool {
        match self.retry_after {
            Some(t) => now >= t,
            None => true,
        }
    }

    pub(crate) fn on_success(&mut self) -> LinkTransition {
        self.retry_after = None;
        match std::mem::replace(&mut self.state, LinkState::Reachable) {
            LinkState::NeverConnected => LinkTransition::Connected,
            LinkState::Unreachable => LinkTransition::Recovered,
            LinkState::Reachable => LinkTransition::None,
        }
    }

    pub(crate) fn on_failure(&mut self, now: Instant) -> LinkTransition {
        self.retry_after = Some(now + self.cooldown);
        match std::mem::replace(&mut self.state, LinkState::Unreachable) {
            LinkState::Unreachable => LinkTransition::None,
            _ => LinkTransition::Lost,
        }
    }

    /// Current belief. `NeverConnected` reads as false: nothing has been
    /// proven reachable yet. (Live reachability surfacing goes through a real
    /// PING in `healthy()`, so production code does not consult this.)
    #[cfg(test)]
    pub(crate) fn is_reachable(&self) -> bool {
        self.state == LinkState::Reachable
    }
}

/// Shared cache for stateless replicas. Entries are JSON values with a Redis
/// TTL, so expiry is enforced by the server rather than by any one process.
///
/// One multiplexed connection is held and reused across operations (it is a
/// cheap-to-clone handle over a single socket + driver task). On any Redis
/// error the held connection is dropped and the next operation re-establishes
/// it, gated by [`ReconnectGate`] so a dead Redis never turns into a
/// per-request connect storm. Operations stay fail-open throughout: a broken
/// link means "miss" / "skip store", never a failed request.
pub struct RedisStore {
    client: redis::Client,
    namespace: String,
    conn: RwLock<Option<MultiplexedConnection>>,
    gate: std::sync::Mutex<ReconnectGate>,
}

impl RedisStore {
    pub fn new(url: &str, namespace: &str) -> anyhow::Result<Self> {
        Self::with_cooldown(url, namespace, REDIS_RETRY_COOLDOWN)
    }

    /// Test seam: the reconnect cooldown is time-based, so tests shrink it
    /// rather than sleeping through the production window.
    pub(crate) fn with_cooldown(
        url: &str,
        namespace: &str,
        cooldown: Duration,
    ) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        Ok(Self {
            client,
            namespace: namespace.to_string(),
            conn: RwLock::new(None),
            gate: std::sync::Mutex::new(ReconnectGate::new(cooldown)),
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}:{}", self.namespace, key)
    }

    /// Hand out the held connection, (re)establishing it if necessary.
    ///
    /// `None` means "behave as a miss": either the gate is in its cooldown
    /// window, another task is already dialling, or the dial just failed.
    async fn conn(&self) -> Option<MultiplexedConnection> {
        if let Some(c) = self.conn.read().await.as_ref() {
            return Some(c.clone());
        }
        // No held connection. `try_write` rather than `write`: if another task
        // is already dialling, this operation misses immediately instead of
        // queueing behind the connect attempt.
        let Ok(mut guard) = self.conn.try_write() else {
            return None;
        };
        if let Some(c) = guard.as_ref() {
            // Lost the race to a task that just connected — use its work.
            return Some(c.clone());
        }
        if !self.gate.lock().unwrap().should_attempt(Instant::now()) {
            return None;
        }
        // The redis crate applies bounded defaults here (1s connect, 500ms
        // response), so holding the write lock across the dial is capped.
        match self.client.get_multiplexed_async_connection().await {
            Ok(c) => {
                let transition = self.gate.lock().unwrap().on_success();
                match transition {
                    LinkTransition::Connected => {
                        tracing::info!(namespace = %self.namespace, "redis cache connected");
                    }
                    LinkTransition::Recovered => {
                        tracing::info!(
                            namespace = %self.namespace,
                            "redis cache is reachable again; resuming caching"
                        );
                    }
                    _ => {}
                }
                *guard = Some(c.clone());
                Some(c)
            }
            Err(e) => {
                self.note_failure("connect", &e);
                None
            }
        }
    }

    /// Drop the held connection so the next operation re-establishes it, and
    /// log the reachable -> unreachable transition exactly once.
    async fn mark_broken(&self, op: &'static str, err: &redis::RedisError) {
        *self.conn.write().await = None;
        self.note_failure(op, err);
    }

    fn note_failure(&self, op: &'static str, err: &redis::RedisError) {
        let (transition, cooldown) = {
            let mut gate = self.gate.lock().unwrap();
            (gate.on_failure(Instant::now()), gate.cooldown)
        };
        if transition == LinkTransition::Lost {
            tracing::warn!(
                error = %err,
                op,
                namespace = %self.namespace,
                cooldown_secs = cooldown.as_secs(),
                "redis cache became unreachable; serving misses (requests are unaffected) \
                 and retrying with backoff until it returns"
            );
        } else {
            tracing::debug!(error = %err, op, "redis cache still unreachable; treating as miss");
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
                    self.mark_broken("scan", &e).await;
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
                Err(e) => {
                    // Remaining chunks would ride the same broken link — stop.
                    self.mark_broken("del", &e).await;
                    break;
                }
            }
        }
        removed
    }
}

#[async_trait]
impl CacheStore for RedisStore {
    async fn get(&self, key: &str) -> Option<CachedEntry> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = match redis::cmd("GET")
            .arg(self.full_key(key))
            .query_async(&mut conn)
            .await
        {
            Ok(raw) => raw,
            Err(e) => {
                self.mark_broken("get", &e).await;
                return None;
            }
        };
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
            self.mark_broken("set", &e).await;
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

    /// A real PING over the held connection, so a health probe both reports
    /// AND heals the link: if Redis came back, the probe reconnects and the
    /// next cache operation benefits. During the backoff window this is an
    /// immediate `false` with no network I/O.
    async fn healthy(&self) -> bool {
        let Some(mut conn) = self.conn().await else {
            return false;
        };
        match redis::cmd("PING").query_async::<String>(&mut conn).await {
            Ok(_) => true,
            Err(e) => {
                self.mark_broken("ping", &e).await;
                false
            }
        }
    }
}

pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ReconnectGate: the pure decision logic ────────────────────────────────

    #[test]
    fn gate_allows_first_attempt_and_reports_initial_connect() {
        let mut gate = ReconnectGate::new(Duration::from_secs(5));
        let now = Instant::now();
        assert!(gate.should_attempt(now));
        assert!(!gate.is_reachable());
        assert_eq!(gate.on_success(), LinkTransition::Connected);
        assert!(gate.is_reachable());
        // A second success is not a transition — nothing to log.
        assert_eq!(gate.on_success(), LinkTransition::None);
    }

    #[test]
    fn gate_reports_lost_once_then_stays_quiet() {
        let mut gate = ReconnectGate::new(Duration::from_secs(5));
        gate.on_success();
        let now = Instant::now();
        assert_eq!(gate.on_failure(now), LinkTransition::Lost);
        // Repeated failures while already down must not re-announce.
        assert_eq!(gate.on_failure(now), LinkTransition::None);
        assert!(!gate.is_reachable());
    }

    #[test]
    fn gate_blocks_attempts_during_cooldown_and_reopens_after() {
        let mut gate = ReconnectGate::new(Duration::from_secs(5));
        let now = Instant::now();
        gate.on_failure(now);
        // Inside the window: no dialling, operations are immediate misses.
        assert!(!gate.should_attempt(now));
        assert!(!gate.should_attempt(now + Duration::from_secs(4)));
        // At/after the deadline: exactly one attempt is allowed again.
        assert!(gate.should_attempt(now + Duration::from_secs(5)));
    }

    #[test]
    fn gate_reports_recovery_once() {
        let mut gate = ReconnectGate::new(Duration::from_secs(5));
        gate.on_success();
        gate.on_failure(Instant::now());
        assert_eq!(gate.on_success(), LinkTransition::Recovered);
        assert_eq!(gate.on_success(), LinkTransition::None);
        assert!(gate.is_reachable());
    }

    #[test]
    fn gate_failure_before_any_success_still_announces_lost() {
        // Redis down at process start must produce one loud line, not silence.
        let mut gate = ReconnectGate::new(Duration::from_secs(5));
        assert_eq!(gate.on_failure(Instant::now()), LinkTransition::Lost);
        assert_eq!(gate.on_failure(Instant::now()), LinkTransition::None);
    }

    // ── RedisStore against a fake RESP server ─────────────────────────────────
    //
    // A minimal TCP server that answers "+OK\r\n" per received command frame is
    // enough for connection setup and PING (`healthy()` only requires a simple
    // string reply). This exercises the full reconnect path — held connection,
    // drop-on-error, cooldown, re-establish — without a Redis install.

    struct FakeRedis {
        port: u16,
        shutdown: tokio::sync::watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeRedis {
        async fn start(port: u16) -> FakeRedis {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .expect("bind fake redis");
            let port = listener.local_addr().unwrap().port();
            let (shutdown, watch) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(async move {
                let mut conns: Vec<tokio::task::JoinHandle<()>> = Vec::new();
                let mut watch_accept = watch.clone();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let Ok((mut sock, _)) = accepted else { break };
                            let mut watch_conn = watch.clone();
                            conns.push(tokio::spawn(async move {
                                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                                let mut buf = [0u8; 1024];
                                loop {
                                    tokio::select! {
                                        read = sock.read(&mut buf) => {
                                            let Ok(n) = read else { return };
                                            if n == 0 { return; }
                                            // One reply per RESP command frame
                                            // ('*' opens each client command).
                                            let frames =
                                                buf[..n].iter().filter(|b| **b == b'*').count().max(1);
                                            for _ in 0..frames {
                                                if sock.write_all(b"+OK\r\n").await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                        _ = watch_conn.changed() => return,
                                    }
                                }
                            }));
                        }
                        _ = watch_accept.changed() => break,
                    }
                }
                // Closing accepted sockets makes the client's next command fail
                // rather than hang, mimicking a real server going away.
                for c in conns {
                    c.abort();
                }
            });
            FakeRedis { port, shutdown, task }
        }

        async fn stop(self) -> u16 {
            let _ = self.shutdown.send(true);
            let _ = self.task.await;
            self.port
        }
    }

    #[tokio::test]
    async fn redis_store_reconnects_after_server_restart() {
        let server = FakeRedis::start(0).await;
        let port = server.port;
        let store = RedisStore::with_cooldown(
            &format!("redis://127.0.0.1:{}", port),
            "test-ns",
            Duration::ZERO, // no cooldown: recovery is observable immediately
        )
        .unwrap();

        assert!(store.healthy().await, "fresh server should be reachable");

        // Server goes away: the held connection breaks, healthy() turns false
        // without hanging, and operations degrade to misses.
        let port = server.stop().await;
        assert!(!store.healthy().await, "dead server must read unreachable");
        assert!(store.get("k").await.is_none(), "operations fail open as misses");

        // Server returns on the same port: with the cooldown elapsed (zero),
        // the store must re-establish on the next probe — the exact behavior
        // whose absence latched the incident banner.
        let server = FakeRedis::start(port).await;
        assert!(
            store.healthy().await,
            "store must reconnect once the server is back"
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn redis_store_backoff_blocks_redial_inside_cooldown() {
        let server = FakeRedis::start(0).await;
        let port = server.port;
        let store = RedisStore::with_cooldown(
            &format!("redis://127.0.0.1:{}", port),
            "test-ns",
            Duration::from_secs(60), // longer than the test — never reopens
        )
        .unwrap();

        assert!(store.healthy().await);
        let port = server.stop().await;
        assert!(!store.healthy().await);

        // The server is back, but the cooldown is armed: the store must NOT
        // dial (a dead Redis becoming a per-request connect storm is what the
        // cooldown prevents), so it still reads unreachable.
        let server = FakeRedis::start(port).await;
        assert!(
            !store.healthy().await,
            "no redial inside the cooldown window"
        );
        assert!(store.get("k").await.is_none(), "still an immediate miss");
        server.stop().await;
    }

    // ── Live Redis (opt-in) ───────────────────────────────────────────────────
    //
    // Requires a Redis at 127.0.0.1:6379. Uses (and purges) only its own
    // scratch namespace. Run with: cargo test redis_live -- --ignored

    #[tokio::test]
    #[ignore]
    async fn redis_live_round_trip_scratch_namespace() {
        let store = RedisStore::new("redis://127.0.0.1:6379", "mr-test-scratch").unwrap();
        assert!(store.healthy().await, "live Redis should be reachable");
        let entry = CachedEntry {
            class: "completion".to_string(),
            model: "test-model".to_string(),
            payload: serde_json::json!({"content": "live round trip"}),
            original_cost_usd: 0.0,
            stored_at: 0,
            expires_at: 0,
        };
        store.put("live-test-key", entry, Duration::from_secs(30)).await;
        let got = store.get("live-test-key").await.expect("entry should round-trip");
        assert_eq!(got.payload["content"], "live round trip");
        assert!(store.entry_count().await >= 1);
        // Clean up ONLY the scratch namespace.
        assert!(store.purge_all().await >= 1);
        assert_eq!(store.entry_count().await, 0);
    }
}
