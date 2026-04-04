# Phase 11b: Exact-Match Cache + Embeddings Route

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an in-memory LRU+TTL response cache for `POST /v1/chat/completions` (Task 11.2), and a new `POST /v1/embeddings` endpoint with an OpenAI adapter (Task 11.4).

**Architecture:** The response cache sits between auth and policy — a cache hit returns immediately with no cost logged. The embeddings route follows the same auth → policy → provider → cost pattern as completions, using a new `EmbeddingAdapter` trait and `EmbeddingRegistry` that mirrors `ProviderRegistry`. Task 11.3 (semantic cache with Qdrant) is deferred.

**Tech Stack:** Rust 2021, axum 0.7, moka 0.12 (async LRU+TTL), reqwest 0.12, sha2/hex, serde_json, tokio

---

## Scope note

Phase 11b covers Tasks 11.2 and 11.4 only. Task 11.3 (semantic cache) requires Qdrant (external service dependency) and is deferred.

---

## File Map

### Task 1 — Exact-Match Response Cache

| File | Action | Responsibility |
|------|--------|----------------|
| `Cargo.toml` | Modify | Add `moka = { version = "0.12", features = ["future"] }` |
| `src/config/schema.rs` | Modify | Add `CacheConfig` struct and `cache: CacheConfig` field on `Settings` |
| `src/router/cache.rs` | Create | `ResponseCache` struct wrapping `moka::future::Cache`, `make_cache_key()` |
| `src/router/mod.rs` | Modify | Declare `pub mod cache;` |
| `src/api/app.rs` | Modify | Add `response_cache: Arc<ResponseCache>` to `AppState` |
| `src/api/routes/completions.rs` | Modify | Check cache before policy; store in cache after successful non-streaming response |
| `src/cli/mod.rs` | Modify | Construct `ResponseCache` from settings, inject into `AppState` |
| `tests/test_cache.rs` | Create | Unit tests for `ResponseCache` and integration test for cache hit |
| All test files that construct `AppState` | Modify | Add `response_cache` field |

### Task 2 — Embeddings Route

| File | Action | Responsibility |
|------|--------|----------------|
| `src/providers/embedding.rs` | Create | `EmbeddingRequest`, `EmbeddingResult`, `EmbeddingAdapter` trait |
| `src/providers/openai_embed.rs` | Create | `OpenAIEmbeddingAdapter` implementing `EmbeddingAdapter` |
| `src/providers/embed_registry.rs` | Create | `EmbeddingRegistry` — lazy adapter construction, `new_with_mock` for tests |
| `src/providers/mod.rs` | Modify | Declare new modules |
| `src/api/routes/embeddings.rs` | Create | `POST /v1/embeddings` handler |
| `src/api/routes/mod.rs` | Modify | Declare `pub mod embeddings;` |
| `src/api/app.rs` | Modify | Add `embedding_registry: Arc<EmbeddingRegistry>` to `AppState`; register route |
| `src/cli/mod.rs` | Modify | Construct `EmbeddingRegistry` from settings, inject into `AppState` |
| `tests/common/mod.rs` | Modify | Add `MockEmbeddingAdapter` |
| `tests/test_embeddings.rs` | Create | Integration tests for embeddings route |
| All test files that construct `AppState` | Modify | Add `embedding_registry` field |

---

## Task 1: Exact-Match Response Cache

**Files:**
- Modify: `Cargo.toml`
- Create: `src/router/cache.rs`
- Modify: `src/config/schema.rs`
- Modify: `src/router/mod.rs`
- Modify: `src/api/app.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `src/cli/mod.rs`
- Create: `tests/test_cache.rs`
- Modify all test files that construct `AppState`

---

- [ ] **Step 1: Write failing tests for ResponseCache and cache key generation**

Create `tests/test_cache.rs`:

```rust
use modelrouter::router::cache::{make_cache_key, ResponseCache};
use modelrouter::config::schema::CacheConfig;
use modelrouter::providers::adapter::CompletionResult;
use serde_json::json;

fn enabled_cache(max_entries: u64, ttl_seconds: u64) -> ResponseCache {
    ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries,
        ttl_seconds,
    })
}

#[tokio::test]
async fn cache_miss_returns_none() {
    let cache = enabled_cache(100, 60);
    assert!(cache.get("nonexistent-key").await.is_none());
}

#[tokio::test]
async fn cache_hit_returns_value() {
    let cache = enabled_cache(100, 60);
    let result = CompletionResult {
        content: "cached!".to_string(),
        prompt_tokens: 5,
        completion_tokens: 3,
        finish_reason: "stop".to_string(),
    };
    cache.insert("key-1".to_string(), result.clone()).await;
    let hit = cache.get("key-1").await.unwrap();
    assert_eq!(hit.content, "cached!");
    assert_eq!(hit.prompt_tokens, 5);
}

#[tokio::test]
async fn disabled_cache_always_misses() {
    let cache = ResponseCache::new(&CacheConfig {
        enabled: false,
        max_entries: 100,
        ttl_seconds: 60,
    });
    let result = CompletionResult {
        content: "ignored".to_string(),
        prompt_tokens: 1,
        completion_tokens: 1,
        finish_reason: "stop".to_string(),
    };
    cache.insert("key".to_string(), result).await;
    assert!(cache.get("key").await.is_none());
}

#[test]
fn same_inputs_produce_same_key() {
    let messages = vec![json!({"role": "user", "content": "hello"})];
    let k1 = make_cache_key("gpt-4o", &messages, Some(0.7), Some(100));
    let k2 = make_cache_key("gpt-4o", &messages, Some(0.7), Some(100));
    assert_eq!(k1, k2);
}

#[test]
fn different_model_produces_different_key() {
    let messages = vec![json!({"role": "user", "content": "hello"})];
    let k1 = make_cache_key("gpt-4o", &messages, None, None);
    let k2 = make_cache_key("gpt-4o-mini", &messages, None, None);
    assert_ne!(k1, k2);
}

#[test]
fn different_messages_produce_different_key() {
    let m1 = vec![json!({"role": "user", "content": "hello"})];
    let m2 = vec![json!({"role": "user", "content": "world"})];
    let k1 = make_cache_key("gpt-4o", &m1, None, None);
    let k2 = make_cache_key("gpt-4o", &m2, None, None);
    assert_ne!(k1, k2);
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --test test_cache 2>&1 | head -20
```

Expected: compile error — module `cache` not found.

- [ ] **Step 3: Add `moka` to `Cargo.toml`**

In `[dependencies]`, add:

```toml
moka = { version = "0.12", features = ["future"] }
```

- [ ] **Step 4: Add `CacheConfig` to `src/config/schema.rs`**

Add after `PricingEntry`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: u64,
    #[serde(default = "default_cache_ttl")]
    pub ttl_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: default_cache_max_entries(),
            ttl_seconds: default_cache_ttl(),
        }
    }
}

fn default_cache_max_entries() -> u64 { 1000 }
fn default_cache_ttl() -> u64 { 3600 }
```

Add to `Settings`:

```rust
pub struct Settings {
    // ... existing fields ...
    #[serde(default)]
    pub cache: CacheConfig,
}
```

- [ ] **Step 5: Create `src/router/cache.rs`**

```rust
use serde_json::Value;
use crate::config::schema::CacheConfig;
use crate::providers::adapter::CompletionResult;

pub struct ResponseCache {
    inner: Option<moka::future::Cache<String, CompletionResult>>,
}

impl ResponseCache {
    pub fn new(config: &CacheConfig) -> Self {
        if !config.enabled {
            return Self { inner: None };
        }
        let cache = moka::future::Cache::builder()
            .max_capacity(config.max_entries)
            .time_to_live(std::time::Duration::from_secs(config.ttl_seconds))
            .build();
        Self { inner: Some(cache) }
    }

    pub async fn get(&self, key: &str) -> Option<CompletionResult> {
        self.inner.as_ref()?.get(key).await
    }

    pub async fn insert(&self, key: String, value: CompletionResult) {
        if let Some(ref cache) = self.inner {
            cache.insert(key, value).await;
        }
    }
}

/// Build a deterministic cache key from request parameters.
/// Returns a hex-encoded SHA-256 of the canonicalized inputs.
pub fn make_cache_key(
    model: &str,
    messages: &[Value],
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> String {
    use sha2::{Digest, Sha256};
    let payload = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": temperature,
        "max_tokens": max_tokens,
    });
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(&payload)
            .unwrap_or_default()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}
```

- [ ] **Step 6: Declare `pub mod cache;` in `src/router/mod.rs`**

Add `pub mod cache;` to the existing list.

- [ ] **Step 7: Run cache unit tests**

```bash
cargo test --test test_cache
```

Expected: all 7 tests pass.

- [ ] **Step 8: Add `response_cache` to `AppState`**

In `src/api/app.rs`, add to `AppState`:

```rust
pub response_cache: Arc<crate::router::cache::ResponseCache>,
```

- [ ] **Step 9: Wire cache into `chat_completions_inner` in `src/api/routes/completions.rs`**

After extracting `model` (after complexity router downgrade) and before the hooks/policy block, add:

```rust
let stream = body["stream"].as_bool().unwrap_or(false);

// Build cache key for non-streaming requests only
let cache_key = if !stream {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    Some(crate::router::cache::make_cache_key(
        &model,
        &msgs,
        body["temperature"].as_f64(),
        body["max_tokens"].as_u64().map(|v| v as u32),
    ))
} else {
    None
};

// Check cache — hit returns immediately with no policy check or cost
if let Some(ref key) = cache_key {
    if let Some(cached) = state.response_cache.get(key).await {
        tracing::info!(cache_key = key.as_str(), model = model.as_str(), "response cache hit");
        let request_id = format!("chatcmpl-mr-{}", uuid::Uuid::new_v4());
        return Ok(Json(build_openai_response(request_id, &cached)).into_response());
    }
}
```

Note: `stream` is already extracted later in the function — move that extraction earlier (before the cache key block) or duplicate the `let stream = ...` line. The simplest approach is to move `let stream = ...` to before the cache key block.

After a successful non-streaming completion (after the result loop), before building the response, store in cache:

```rust
// Store result in cache for future requests
if let Some(key) = cache_key {
    state.response_cache.insert(key, result.clone()).await;
}
```

Place this just before `Ok(Json(build_openai_response(request_id, &result)).into_response())`.

- [ ] **Step 10: Construct `ResponseCache` in `src/cli/mod.rs`**

Find where `AppState` is constructed and add:

```rust
let response_cache = Arc::new(crate::router::cache::ResponseCache::new(&settings.cache));
```

Add `response_cache` to the `AppState { ... }` initializer.

- [ ] **Step 11: Update all test files that construct `AppState`**

Each test file that builds `AppState` manually needs:

```rust
let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
    &modelrouter::config::schema::CacheConfig::default()
));
```

and `response_cache` added to the `AppState { ... }` struct literal.

Test files to update: `tests/test_completions.rs`, `tests/test_messages.rs`, `tests/test_dashboard.rs`, `tests/test_prometheus.rs`, `tests/test_telemetry.rs`, `tests/test_router.rs`, `tests/test_per_key_budgets.rs`.

- [ ] **Step 12: Add integration test for cache hit in `tests/test_cache.rs`**

Add to `tests/test_cache.rs` (this file now needs `mod common` and `AppState` setup, so extend it):

```rust
mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, Settings};
use modelrouter::db::models::NewUser;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    cache::ResponseCache,
    complexity::ComplexityRouter,
    cost::CostCalculator,
    engine::RequestRouter,
    fallback::FallbackChain,
    policy::PolicyEngine,
};
use std::collections::HashMap;
use std::sync::Arc;

// ... (keep the unit tests above) ...

async fn test_app_with_cache() -> TestServer {
    let db = common::in_memory_db().await;
    db.create(NewUser {
        name: "test-user".to_string(),
        api_key_hash: hash_token("test-token"),
        group_name: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "cached response".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(ResponseCache::new(&CacheConfig {
        enabled: true,
        max_entries: 10,
        ttl_seconds: 60,
    }));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1, 0.2] },
        )
    );

    let state = AppState {
        settings,
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        app_metrics: None,
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn second_identical_request_returns_cached_response() {
    let server = test_app_with_cache().await;
    let body = serde_json::json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hello cache"}]
    });

    // First request — goes to provider
    let resp1 = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&body)
        .await;
    assert_eq!(resp1.status_code(), 200);

    // Second identical request — should hit cache
    let resp2 = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&body)
        .await;
    assert_eq!(resp2.status_code(), 200);

    // Both responses contain the same content
    let b1: serde_json::Value = resp1.json();
    let b2: serde_json::Value = resp2.json();
    assert_eq!(
        b1["choices"][0]["message"]["content"],
        b2["choices"][0]["message"]["content"]
    );
}

#[tokio::test]
async fn streaming_requests_are_not_cached() {
    let server = test_app_with_cache().await;
    // Streaming request — cache key should not be generated, no interference
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "stream me"}],
            "stream": true
        }))
        .await;
    // Streaming still succeeds (200) — just not cached
    assert_eq!(resp.status_code(), 200);
}
```

- [ ] **Step 13: Build and run all tests**

```bash
cargo build && cargo test
```

Expected: all tests pass.

- [ ] **Step 14: Commit**

```bash
git add Cargo.toml Cargo.lock \
        src/config/schema.rs \
        src/router/cache.rs \
        src/router/mod.rs \
        src/api/app.rs \
        src/api/routes/completions.rs \
        src/cli/mod.rs \
        tests/test_cache.rs \
        tests/test_completions.rs tests/test_messages.rs tests/test_dashboard.rs \
        tests/test_prometheus.rs tests/test_telemetry.rs tests/test_router.rs \
        tests/test_per_key_budgets.rs
git commit -m "feat: add exact-match response cache with LRU+TTL — cache hits skip policy and cost recording"
```

---

## Task 2: Embeddings Route

**Files:**
- Create: `src/providers/embedding.rs`
- Create: `src/providers/openai_embed.rs`
- Create: `src/providers/embed_registry.rs`
- Modify: `src/providers/mod.rs`
- Create: `src/api/routes/embeddings.rs`
- Modify: `src/api/routes/mod.rs`
- Modify: `src/api/app.rs`
- Modify: `src/cli/mod.rs`
- Modify: `tests/common/mod.rs`
- Create: `tests/test_embeddings.rs`
- Modify all test files that construct `AppState`

---

- [ ] **Step 1: Write failing integration test**

Create `tests/test_embeddings.rs`:

```rust
mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::schema::{CacheConfig, Settings};
use modelrouter::db::models::NewUser;
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::providers::{
    embed_registry::EmbeddingRegistry,
    registry::ProviderRegistry,
};
use modelrouter::router::{
    cache::ResponseCache,
    complexity::ComplexityRouter,
    cost::CostCalculator,
    engine::RequestRouter,
    fallback::FallbackChain,
    policy::PolicyEngine,
};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> TestServer {
    let db = common::in_memory_db().await;
    db.create(NewUser {
        name: "test-user".to_string(),
        api_key_hash: hash_token("test-token"),
        group_name: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "hello".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));
    let response_cache = Arc::new(ResponseCache::new(&CacheConfig::default()));
    let embedding_registry = Arc::new(EmbeddingRegistry::new_with_mock(
        common::MockEmbeddingAdapter {
            embedding: vec![0.1_f32, 0.2, 0.3],
        },
    ));

    let state = AppState {
        settings,
        db,
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        response_cache,
        embedding_registry,
        app_metrics: None,
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn embeddings_unauthenticated_returns_401() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "hello world"
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn embeddings_string_input_returns_200() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": "hello world"
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["object"], "list");
    assert!(body["data"].is_array());
    assert_eq!(body["data"][0]["object"], "embedding");
    assert!(body["data"][0]["embedding"].is_array());
}

#[tokio::test]
async fn embeddings_array_input_returns_one_entry_per_string() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": ["hello", "world"]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: serde_json::Value = resp.json();
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"][0]["index"], 0);
    assert_eq!(body["data"][1]["index"], 1);
}

#[tokio::test]
async fn embeddings_invalid_input_type_returns_400() {
    let server = test_app().await;
    let resp = server
        .post("/v1/embeddings")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "text-embedding-3-small",
            "input": 42
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test --test test_embeddings 2>&1 | head -20
```

Expected: compile errors — `embed_registry`, `EmbeddingRegistry`, `MockEmbeddingAdapter` not found.

- [ ] **Step 3: Create `src/providers/embedding.rs`**

```rust
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingResult {
    pub embeddings: Vec<Vec<f32>>,
    pub prompt_tokens: u32,
}

#[async_trait]
pub trait EmbeddingAdapter: Send + Sync {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult>;
}
```

- [ ] **Step 4: Create `src/providers/openai_embed.rs`**

```rust
use anyhow::Context;
use crate::config::schema::ProviderConfig;
use crate::providers::embedding::{EmbeddingAdapter, EmbeddingRequest, EmbeddingResult};

pub struct OpenAIEmbeddingAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
}

impl OpenAIEmbeddingAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to build reqwest client");
        Self {
            api_key: config.api_key.clone(),
            api_base,
            client,
        }
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    usage: EmbeddingUsage,
}

#[derive(serde::Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[derive(serde::Deserialize)]
struct EmbeddingUsage {
    prompt_tokens: u32,
}

#[async_trait::async_trait]
impl EmbeddingAdapter for OpenAIEmbeddingAdapter {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult> {
        let url = format!("{}/embeddings", self.api_base);

        // OpenAI accepts either a single string or array; always send array
        let body = serde_json::json!({
            "model": req.model,
            "input": req.input,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send embedding request to OpenAI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding provider returned {}: {}", status, text);
        }

        let parsed: EmbeddingResponse = resp
            .json()
            .await
            .context("Failed to parse embedding response")?;

        Ok(EmbeddingResult {
            embeddings: parsed.data.into_iter().map(|d| d.embedding).collect(),
            prompt_tokens: parsed.usage.prompt_tokens,
        })
    }
}
```

- [ ] **Step 5: Create `src/providers/embed_registry.rs`**

```rust
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use crate::config::schema::ProviderConfig;
use crate::providers::embedding::EmbeddingAdapter;

pub struct EmbeddingRegistry {
    adapters: DashMap<String, Arc<dyn EmbeddingAdapter>>,
    configs: HashMap<String, ProviderConfig>,
}

impl EmbeddingRegistry {
    pub fn new(configs: HashMap<String, ProviderConfig>) -> Self {
        Self {
            adapters: DashMap::new(),
            configs,
        }
    }

    pub fn get(&self, provider_name: &str) -> anyhow::Result<Arc<dyn EmbeddingAdapter>> {
        if let Some(adapter) = self.adapters.get(provider_name) {
            return Ok(adapter.clone());
        }

        // Fall back to first available adapter (useful in tests)
        if self.configs.is_empty() {
            if let Some(entry) = self.adapters.iter().next() {
                return Ok(entry.value().clone());
            }
        }

        let config = self
            .configs
            .get(provider_name)
            .ok_or_else(|| anyhow::anyhow!("No embedding adapter for provider: {}", provider_name))?;

        let adapter: Arc<dyn EmbeddingAdapter> = Arc::new(
            crate::providers::openai_embed::OpenAIEmbeddingAdapter::new(config),
        );

        let entry = self
            .adapters
            .entry(provider_name.to_string())
            .or_insert(adapter);
        Ok(entry.clone())
    }

    /// Test helper: create registry with a single mock adapter for any provider.
    pub fn new_with_mock<A: EmbeddingAdapter + 'static>(mock: A) -> Self {
        let registry = Self {
            adapters: DashMap::new(),
            configs: HashMap::new(),
        };
        let mock_arc: Arc<dyn EmbeddingAdapter> = Arc::new(mock);
        registry.adapters.insert("__mock__".to_string(), mock_arc);
        registry
    }
}
```

- [ ] **Step 6: Add `MockEmbeddingAdapter` to `tests/common/mod.rs`**

Add after the existing `MockAdapter`:

```rust
pub struct MockEmbeddingAdapter {
    pub embedding: Vec<f32>,
}

#[async_trait::async_trait]
impl modelrouter::providers::embedding::EmbeddingAdapter for MockEmbeddingAdapter {
    async fn embed(
        &self,
        req: &modelrouter::providers::embedding::EmbeddingRequest,
    ) -> anyhow::Result<modelrouter::providers::embedding::EmbeddingResult> {
        Ok(modelrouter::providers::embedding::EmbeddingResult {
            embeddings: vec![self.embedding.clone(); req.input.len()],
            prompt_tokens: req.input.iter().map(|s| s.len() as u32 / 4).sum(),
        })
    }
}
```

- [ ] **Step 7: Declare new modules in `src/providers/mod.rs`**

```rust
pub mod adapter;
pub mod anthropic;
pub mod embed_registry;
pub mod embedding;
pub mod openai_compat;
pub mod openai_embed;
pub mod registry;
```

- [ ] **Step 8: Create `src/api/routes/embeddings.rs`**

```rust
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    providers::embedding::EmbeddingRequest,
    router::policy::PolicyDecision,
};

pub async fn embeddings(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let model = body["model"]
        .as_str()
        .unwrap_or("text-embedding-3-small")
        .to_string();

    // Policy check
    let policy_result = state
        .policy
        .check(&user, &model)
        .await
        .map_err(|_| ApiError::Internal)?;
    match policy_result {
        PolicyDecision::Allow => {}
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // Parse input — accepts either a single string or an array of strings
    let input: Vec<String> = match &body["input"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => {
            return Err(ApiError::InvalidRequest(
                "input must be a string or array of strings".to_string(),
            ))
        }
    };

    if input.is_empty() {
        return Err(ApiError::InvalidRequest("input must not be empty".to_string()));
    }

    let (provider_name, canonical_model) = state.router.resolve(&model);
    let adapter = state
        .embedding_registry
        .get(&provider_name)
        .map_err(ApiError::ProviderError)?;

    let req = EmbeddingRequest {
        model: canonical_model.clone(),
        input,
    };

    let result = adapter.embed(&req).await.map_err(ApiError::ProviderError)?;

    let cost = state
        .cost_calc
        .calculate(&canonical_model, result.prompt_tokens, 0);

    // Fire-and-forget cost recording
    let state_clone = state.clone();
    let model_clone = model.clone();
    let canonical_clone = canonical_model.clone();
    let provider_clone = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let prompt_tokens = result.prompt_tokens;

    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id,
            session_id: None,
            request_model: model_clone,
            routed_model: canonical_clone.clone(),
            provider: provider_clone.clone(),
            messages: "[]".to_string(), // embeddings have no chat messages
            response: None,
            finish_reason: None,
            prompt_tokens: prompt_tokens as i64,
            completion_tokens: 0,
            cost_usd: cost,
            latency_ms: None,
            tags: "[]".to_string(),
            project: None,
        };
        match PromptRepository::create(&*state_clone.db, prompt).await {
            Ok(saved) => {
                let ledger = NewCostLedgerEntry {
                    user_id,
                    prompt_id: saved.id,
                    model: canonical_clone,
                    provider: provider_clone,
                    project: None,
                    tokens_in: prompt_tokens as i64,
                    tokens_out: 0,
                    cost_usd: cost,
                    api_key_id,
                };
                if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
                    tracing::error!("Failed to record embedding cost: {}", e);
                }
            }
            Err(e) => tracing::error!("Failed to record embedding prompt: {}", e),
        }
    });

    // Build OpenAI-compatible response
    let data: Vec<Value> = result
        .embeddings
        .iter()
        .enumerate()
        .map(|(i, emb)| {
            serde_json::json!({
                "object": "embedding",
                "index": i,
                "embedding": emb,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "object": "list",
        "data": data,
        "model": canonical_model,
        "usage": {
            "prompt_tokens": result.prompt_tokens,
            "total_tokens": result.prompt_tokens,
        }
    }))
    .into_response())
}
```

- [ ] **Step 9: Declare `pub mod embeddings;` in `src/api/routes/mod.rs`**

Check if `src/api/routes/mod.rs` exists; if not, the route modules are declared in `src/api/mod.rs`. Find where `pub mod completions;` is declared and add `pub mod embeddings;` alongside it.

- [ ] **Step 10: Add `embedding_registry` to `AppState` and register route**

In `src/api/app.rs`:

Add to `AppState`:
```rust
pub embedding_registry: Arc<crate::providers::embed_registry::EmbeddingRegistry>,
```

In `build_router`, add:
```rust
use crate::api::routes::embeddings::embeddings;
// ...
.route("/v1/embeddings", post(embeddings))
```

- [ ] **Step 11: Construct `EmbeddingRegistry` in `src/cli/mod.rs`**

```rust
let embedding_registry = Arc::new(crate::providers::embed_registry::EmbeddingRegistry::new(
    settings.providers.clone()
));
```

Add `embedding_registry` to the `AppState { ... }` initializer.

- [ ] **Step 12: Update all test files that construct `AppState`**

Each test file that builds `AppState` needs:

```rust
let embedding_registry = Arc::new(
    modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
        common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] }
    )
);
```

and `embedding_registry` added to the `AppState { ... }` struct literal.

Test files: `tests/test_completions.rs`, `tests/test_messages.rs`, `tests/test_dashboard.rs`, `tests/test_prometheus.rs`, `tests/test_telemetry.rs`, `tests/test_router.rs`, `tests/test_per_key_budgets.rs`, `tests/test_cache.rs`.

- [ ] **Step 13: Run tests to confirm they pass**

```bash
cargo test
```

Expected: all tests pass including the 4 new embedding tests.

- [ ] **Step 14: Commit**

```bash
git add src/providers/embedding.rs src/providers/openai_embed.rs \
        src/providers/embed_registry.rs src/providers/mod.rs \
        src/api/routes/embeddings.rs \
        src/api/app.rs src/cli/mod.rs \
        tests/common/mod.rs tests/test_embeddings.rs \
        tests/test_completions.rs tests/test_messages.rs tests/test_dashboard.rs \
        tests/test_prometheus.rs tests/test_telemetry.rs tests/test_router.rs \
        tests/test_per_key_budgets.rs tests/test_cache.rs
git commit -m "feat: add POST /v1/embeddings with OpenAI embedding adapter"
```
