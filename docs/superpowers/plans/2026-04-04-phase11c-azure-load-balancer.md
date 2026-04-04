# Phase 11c: Azure OpenAI Adapter + Load Balancer

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an Azure OpenAI provider adapter (Task 11.6) and a round-robin/weighted load balancer for distributing requests across a configured pool of providers (Task 11.8).

**Architecture:** The Azure adapter mirrors `OpenAICompatAdapter` but uses `api-key` auth and appends `?api-version=` to the URL. The load balancer is a new `LoadBalancer` struct in `src/router/` that maps virtual pool names to ordered provider/model lists; it intercepts model resolution in the completions and messages handlers before `RequestRouter::resolve()`. AWS Bedrock (Task 11.7) is deferred to Phase 11d.

**Tech Stack:** Rust 2021, axum 0.7, reqwest 0.12, `std::sync::atomic::AtomicUsize` for round-robin counter

---

## Scope note

Phase 11c covers Tasks 11.6 (Azure OpenAI) and 11.8 (Load balancer). Task 11.7 (AWS Bedrock — SigV4 + Converse API) is deferred to Phase 11d.

---

## File Map

### Task 1 — Azure OpenAI Adapter

| File | Action | Responsibility |
|------|--------|----------------|
| `src/providers/azure_openai.rs` | Create | `AzureOpenAIAdapter` — `api-key` auth, `?api-version=` URL |
| `src/providers/mod.rs` | Modify | Declare `pub mod azure_openai;` |
| `src/providers/registry.rs` | Modify | Dispatch `"azure"` provider name to `AzureOpenAIAdapter` |
| `src/config/schema.rs` | Modify | Add `api_version: Option<String>` to `ProviderConfig` |
| `tests/test_azure.rs` | Create | Unit tests for Azure adapter URL construction + auth header |

### Task 2 — Load Balancer

| File | Action | Responsibility |
|------|--------|----------------|
| `src/router/load_balancer.rs` | Create | `LoadBalancer` struct, pool selection (round-robin + weighted) |
| `src/router/mod.rs` | Modify | Declare `pub mod load_balancer;` |
| `src/config/schema.rs` | Modify | Add `LoadBalancerConfig`, `LbPoolEntry`, `LbStrategy`; wire into `RoutingConfig` |
| `src/api/app.rs` | Modify | Add `load_balancer: Arc<LoadBalancer>` to `AppState` |
| `src/api/routes/completions.rs` | Modify | Check load balancer before `router.resolve()` |
| `src/api/routes/messages.rs` | Modify | Same load balancer check |
| `src/cli/mod.rs` | Modify | Construct `LoadBalancer` from settings, inject into `AppState` |
| `tests/test_load_balancer.rs` | Create | Unit tests for pool selection strategies |
| All test files that construct `AppState` | Modify | Add `load_balancer` field |

---

## Task 1: Azure OpenAI Adapter

**Files:**
- Create: `src/providers/azure_openai.rs`
- Modify: `src/providers/mod.rs`
- Modify: `src/providers/registry.rs`
- Modify: `src/config/schema.rs`
- Create: `tests/test_azure.rs`

---

- [ ] **Step 1: Write failing tests for Azure adapter URL and auth**

Create `tests/test_azure.rs`. Since `AzureOpenAIAdapter` makes real HTTP calls, test URL construction by checking the config parsing and adapter creation logic:

```rust
use modelrouter::config::schema::{ProviderConfig};
use modelrouter::providers::azure_openai::AzureOpenAIAdapter;

#[test]
fn azure_adapter_builds_correct_url() {
    let config = ProviderConfig {
        api_key: "my-azure-key".to_string(),
        api_base: Some("https://my-resource.openai.azure.com/openai/deployments/my-gpt4".to_string()),
        api_version: Some("2024-02-01".to_string()),
        timeout_secs: 60,
    };
    let adapter = AzureOpenAIAdapter::new(&config);
    assert_eq!(
        adapter.chat_url(),
        "https://my-resource.openai.azure.com/openai/deployments/my-gpt4/chat/completions?api-version=2024-02-01"
    );
}

#[test]
fn azure_adapter_defaults_api_version() {
    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: Some("https://resource.openai.azure.com/openai/deployments/gpt4".to_string()),
        api_version: None, // should default to "2024-02-01"
        timeout_secs: 60,
    };
    let adapter = AzureOpenAIAdapter::new(&config);
    assert!(adapter.chat_url().contains("api-version=2024-02-01"));
}

#[test]
fn azure_adapter_uses_api_base_fallback() {
    // If api_base is None, should use a sensible error-communicating URL
    // (in practice, Azure requires api_base — this just shouldn't panic)
    let config = ProviderConfig {
        api_key: "key".to_string(),
        api_base: None,
        api_version: None,
        timeout_secs: 60,
    };
    let adapter = AzureOpenAIAdapter::new(&config);
    let url = adapter.chat_url();
    assert!(url.contains("api-version="));
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test --test test_azure 2>&1 | head -20
```

Expected: compile error — `azure_openai` module not found.

- [ ] **Step 3: Add `api_version` to `ProviderConfig` in `src/config/schema.rs`**

Add one field to `ProviderConfig`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    pub api_base: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Azure OpenAI API version (e.g. "2024-02-01"). Used only by the Azure adapter.
    pub api_version: Option<String>,
}
```

No default function needed — it's `Option<String>` which deserializes to `None` when absent.

- [ ] **Step 4: Create `src/providers/azure_openai.rs`**

```rust
use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use std::pin::Pin;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

const DEFAULT_API_VERSION: &str = "2024-02-01";

pub struct AzureOpenAIAdapter {
    api_key: String,
    api_base: String,
    api_version: String,
    client: reqwest::Client,
}

impl AzureOpenAIAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let api_base = config
            .api_base
            .clone()
            .unwrap_or_else(|| "https://missing-api-base.openai.azure.com".to_string());
        let api_version = config
            .api_version
            .clone()
            .unwrap_or_else(|| DEFAULT_API_VERSION.to_string());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to build reqwest client");
        Self {
            api_key: config.api_key.clone(),
            api_base,
            api_version,
            client,
        }
    }

    /// Returns the full chat completions URL including api-version query param.
    pub fn chat_url(&self) -> String {
        format!(
            "{}/chat/completions?api-version={}",
            self.api_base, self.api_version
        )
    }

    fn build_body(req: &NormalizedRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": req.messages,
        });
        if let Some(t) = req.temperature {
            body["temperature"] = serde_json::json!(t);
        }
        if let Some(m) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(m);
        }
        if let Some(extra) = &req.extra_params {
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    body[k] = v.clone();
                }
            }
        }
        body
    }
}

#[async_trait]
impl ProviderAdapter for AzureOpenAIAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let body = Self::build_body(req);
        let resp = self
            .client
            .post(self.chat_url())
            .header("api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Azure OpenAI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Azure OpenAI returned {}: {}", status, text);
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Azure OpenAI response")?;

        Ok(CompletionResult {
            content: json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            prompt_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: json["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32,
            finish_reason: json["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("stop")
                .to_string(),
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let mut body = Self::build_body(req);
        body["stream"] = serde_json::json!(true);

        let resp = self
            .client
            .post(self.chat_url())
            .header("api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send streaming request to Azure OpenAI")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Azure OpenAI streaming returned {}: {}", status, text);
        }

        let stream = resp
            .bytes_stream()
            .map_err(|e| anyhow::anyhow!("Stream error: {}", e));
        Ok(Box::pin(stream) as SseStream)
    }
}
```

Note: Look at `src/providers/openai_compat.rs` to confirm the exact shape of `build_body` and the streaming approach — adapt to match.

- [ ] **Step 5: Declare `pub mod azure_openai;` in `src/providers/mod.rs`**

Add alongside existing modules.

- [ ] **Step 6: Add Azure dispatch in `src/providers/registry.rs`**

Find the `get()` method where adapters are constructed. Currently:
```rust
if provider_name == "anthropic" {
    Arc::new(AnthropicAdapter::new(config))
} else {
    Arc::new(OpenAICompatAdapter::new(config))
}
```

Add Azure dispatch:
```rust
if provider_name == "anthropic" {
    Arc::new(crate::providers::anthropic::AnthropicAdapter::new(config))
} else if provider_name == "azure" {
    Arc::new(crate::providers::azure_openai::AzureOpenAIAdapter::new(config))
} else {
    Arc::new(crate::providers::openai_compat::OpenAICompatAdapter::new(config))
}
```

- [ ] **Step 7: Run tests**

```bash
cargo test --test test_azure
```

Expected: all 3 tests pass.

**Before running the full suite:** `tests/test_telemetry.rs` constructs a `ProviderConfig { ... }` struct literal. Adding `api_version` to the struct will cause a compile error there. Find the literal and add `api_version: None` to it.

```bash
grep -n "ProviderConfig {" tests/test_telemetry.rs
```

Then add `api_version: None,` to each struct literal found.

```bash
cargo test
```

Expected: all existing tests continue to pass (adding `api_version: Option<String>` to `ProviderConfig` is backward compatible — it deserializes to `None` when absent).

- [ ] **Step 8: Commit**

```bash
git add src/providers/azure_openai.rs src/providers/mod.rs \
        src/providers/registry.rs src/config/schema.rs \
        tests/test_azure.rs
git commit -m "feat: add Azure OpenAI provider adapter with api-key auth and api-version URL param"
```

---

## Task 2: Load Balancer

**Files:**
- Create: `src/router/load_balancer.rs`
- Modify: `src/router/mod.rs`
- Modify: `src/config/schema.rs`
- Modify: `src/api/app.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `src/api/routes/messages.rs`
- Modify: `src/cli/mod.rs`
- Create: `tests/test_load_balancer.rs`
- Modify: all 8 test files that construct `AppState`

---

- [ ] **Step 1: Write failing unit tests for load balancer pool selection**

Create `tests/test_load_balancer.rs`:

```rust
use modelrouter::router::load_balancer::{LoadBalancer, LoadBalancerConfig, LbPoolEntry, LbStrategy};

fn pool(entries: Vec<(&str, &str, u32)>) -> LoadBalancerConfig {
    LoadBalancerConfig {
        strategy: LbStrategy::RoundRobin,
        pool: entries
            .into_iter()
            .map(|(provider, model, weight)| LbPoolEntry {
                provider: provider.to_string(),
                model: model.to_string(),
                weight,
            })
            .collect(),
    }
}

fn weighted_pool(entries: Vec<(&str, &str, u32)>) -> LoadBalancerConfig {
    LoadBalancerConfig {
        strategy: LbStrategy::Weighted,
        pool: entries
            .into_iter()
            .map(|(provider, model, weight)| LbPoolEntry {
                provider: provider.to_string(),
                model: model.to_string(),
                weight,
            })
            .collect(),
    }
}

#[test]
fn round_robin_cycles_through_all_entries() {
    use std::collections::HashMap;
    let mut pools = HashMap::new();
    pools.insert(
        "my-pool".to_string(),
        pool(vec![
            ("openai", "gpt-4o", 1),
            ("anthropic", "claude-opus-4-5", 1),
        ]),
    );
    let lb = LoadBalancer::new(pools);

    let (p1, m1) = lb.resolve("my-pool").unwrap();
    let (p2, m2) = lb.resolve("my-pool").unwrap();
    let (p3, m3) = lb.resolve("my-pool").unwrap();

    // First two are different
    assert_ne!(p1, p2);
    // Third wraps around to first
    assert_eq!(p1, p3);
    assert_eq!(m1, m3);
    let _ = (m1, m2, m3); // suppress unused
}

#[test]
fn unknown_model_returns_none() {
    use std::collections::HashMap;
    let lb = LoadBalancer::new(HashMap::new());
    assert!(lb.resolve("gpt-4o").is_none());
}

#[test]
fn single_entry_pool_always_returns_same() {
    use std::collections::HashMap;
    let mut pools = HashMap::new();
    pools.insert(
        "single".to_string(),
        pool(vec![("openai", "gpt-4o", 1)]),
    );
    let lb = LoadBalancer::new(pools);
    let first = lb.resolve("single").unwrap();
    let second = lb.resolve("single").unwrap();
    assert_eq!(first, second);
}

#[test]
fn weighted_distributes_proportionally() {
    use std::collections::HashMap;
    let mut pools = HashMap::new();
    pools.insert(
        "weighted".to_string(),
        weighted_pool(vec![
            ("openai", "gpt-4o", 2),
            ("anthropic", "claude-opus-4-5", 1),
        ]),
    );
    let lb = LoadBalancer::new(pools);

    // With weights 2:1, cycle length is 3: openai, openai, anthropic
    let results: Vec<_> = (0..3).map(|_| lb.resolve("weighted").unwrap().0).collect();
    let openai_count = results.iter().filter(|p| p.as_str() == "openai").count();
    let anthropic_count = results.iter().filter(|p| p.as_str() == "anthropic").count();
    assert_eq!(openai_count, 2);
    assert_eq!(anthropic_count, 1);
}

#[test]
fn empty_pool_returns_none() {
    use std::collections::HashMap;
    let mut pools = HashMap::new();
    pools.insert(
        "empty".to_string(),
        LoadBalancerConfig {
            strategy: LbStrategy::RoundRobin,
            pool: vec![],
        },
    );
    let lb = LoadBalancer::new(pools);
    assert!(lb.resolve("empty").is_none());
}
```

- [ ] **Step 2: Run to confirm failure**

```bash
cargo test --test test_load_balancer 2>&1 | head -20
```

Expected: compile error — `load_balancer` module not found.

- [ ] **Step 3: Add load balancer config types to `src/config/schema.rs`**

Add after `ComplexityRoutingConfig`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LbStrategy {
    RoundRobin,
    Weighted,
}

impl Default for LbStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LbPoolEntry {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_lb_weight")]
    pub weight: u32,
}

fn default_lb_weight() -> u32 { 1 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoadBalancerConfig {
    #[serde(default)]
    pub strategy: LbStrategy,
    #[serde(default)]
    pub pool: Vec<LbPoolEntry>,
}
```

Add to `RoutingConfig`:

```rust
pub struct RoutingConfig {
    // ... existing fields ...
    /// Named load balancer pools. Key is the virtual pool name used as `model` in requests.
    /// Example: model_aliases."my-gpt4" = "my-pool" routes to the pool named "my-pool".
    #[serde(default)]
    pub load_balancer: HashMap<String, LoadBalancerConfig>,
}
```

Update `RoutingConfig::default()`:

```rust
impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            default_model: default_model(),
            model_aliases: HashMap::new(),
            fallback_chains: HashMap::new(),
            complexity_routing: None,
            load_balancer: HashMap::new(),
        }
    }
}
```

- [ ] **Step 4: Create `src/router/load_balancer.rs`**

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::schema::{LbStrategy, LoadBalancerConfig};

/// Pool state for one named load balancer pool.
struct Pool {
    /// Expanded entry indices for weighted selection.
    /// For weights [2, 1], expanded = [0, 0, 1].
    expanded: Vec<usize>,
    entries: Vec<(String, String)>, // (provider, model)
    counter: AtomicUsize,
}

impl Pool {
    fn new(config: &LoadBalancerConfig) -> Self {
        let entries: Vec<(String, String)> = config
            .pool
            .iter()
            .map(|e| (e.provider.clone(), e.model.clone()))
            .collect();

        let expanded = match config.strategy {
            LbStrategy::RoundRobin => (0..entries.len()).collect(),
            LbStrategy::Weighted => {
                let mut exp = Vec::new();
                for (i, entry) in config.pool.iter().enumerate() {
                    for _ in 0..entry.weight {
                        exp.push(i);
                    }
                }
                exp
            }
        };

        Self {
            expanded,
            entries,
            counter: AtomicUsize::new(0),
        }
    }

    fn next(&self) -> Option<(String, String)> {
        if self.expanded.is_empty() {
            return None;
        }
        // Use entry API to prevent duplicate creation under concurrency — only first caller wins
        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.expanded.len();
        let entry_idx = self.expanded[idx];
        self.entries.get(entry_idx).cloned()
    }
}

pub struct LoadBalancer {
    pools: HashMap<String, Pool>,
}

impl LoadBalancer {
    /// Construct from a map of pool names to configurations.
    pub fn new(configs: HashMap<String, LoadBalancerConfig>) -> Self {
        let pools = configs
            .into_iter()
            .map(|(name, config)| (name, Pool::new(&config)))
            .collect();
        Self { pools }
    }

    /// If `model` is a named load balancer pool, returns the next `(provider, model)` to use.
    /// Returns `None` if `model` is not a load balancer pool name.
    pub fn resolve(&self, model: &str) -> Option<(String, String)> {
        self.pools.get(model)?.next()
    }
}
```

- [ ] **Step 5: Declare `pub mod load_balancer;` in `src/router/mod.rs`**

Add alongside existing modules.

- [ ] **Step 6: Run unit tests**

```bash
cargo test --test test_load_balancer
```

Expected: all 5 tests pass.

- [ ] **Step 7: Add `load_balancer` to `AppState` in `src/api/app.rs`**

```rust
pub load_balancer: Arc<crate::router::load_balancer::LoadBalancer>,
```

Add after `complexity_router`.

- [ ] **Step 8: Wire load balancer into `chat_completions_inner` in `src/api/routes/completions.rs`**

After the complexity router downgrade produces `model`, and before `state.router.resolve(&model)`, add:

```rust
// Check load balancer: if `model` is a named pool, override provider + model
let (provider_name, canonical_model) = if let Some((lb_provider, lb_model)) =
    state.load_balancer.resolve(&model)
{
    tracing::info!(
        pool = model.as_str(),
        provider = lb_provider.as_str(),
        routed_model = lb_model.as_str(),
        "load balancer selected provider"
    );
    (lb_provider, lb_model)
} else {
    state.router.resolve(&model)
};
```

Note: `completions.rs` currently has something like `let (provider_name, canonical_model) = state.router.resolve(&model);` — replace that line with the block above.

- [ ] **Step 9: Same change in `src/api/routes/messages.rs`**

Apply the identical load balancer interception pattern to the Anthropic messages handler:

```rust
let (provider_name, canonical_model) = if let Some((lb_provider, lb_model)) =
    state.load_balancer.resolve(&model)
{
    tracing::info!(
        pool = model.as_str(),
        provider = lb_provider.as_str(),
        routed_model = lb_model.as_str(),
        "load balancer selected provider"
    );
    (lb_provider, lb_model)
} else {
    state.router.resolve(&model)
};
```

Read `messages.rs` first to confirm where `router.resolve()` is called and what variables are used.

- [ ] **Step 10: Construct `LoadBalancer` in `src/cli/mod.rs`**

```rust
let load_balancer = Arc::new(crate::router::load_balancer::LoadBalancer::new(
    settings.routing.load_balancer.clone(),
));
```

Add `load_balancer` to the `AppState { ... }` initializer.

- [ ] **Step 11: Update all test files that construct `AppState`**

Each test file needs:

```rust
let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
    std::collections::HashMap::new()
));
```

and `load_balancer,` added to the `AppState { ... }` struct literal.

Test files to update:
- `tests/test_completions.rs`
- `tests/test_messages.rs`
- `tests/test_dashboard.rs`
- `tests/test_prometheus.rs`
- `tests/test_per_key_budgets.rs`
- `tests/test_telemetry.rs`
- `tests/test_cache.rs`
- `tests/test_embeddings.rs`

- [ ] **Step 12: Build and run all tests**

```bash
cargo build && cargo test
```

Expected: all tests pass including the 5 new load balancer unit tests.

```bash
cargo build --features otel
```

Expected: clean build.

- [ ] **Step 13: Commit**

```bash
git add src/router/load_balancer.rs src/router/mod.rs \
        src/config/schema.rs \
        src/api/app.rs \
        src/api/routes/completions.rs \
        src/api/routes/messages.rs \
        src/cli/mod.rs \
        tests/test_load_balancer.rs \
        tests/test_completions.rs tests/test_messages.rs tests/test_dashboard.rs \
        tests/test_prometheus.rs tests/test_per_key_budgets.rs tests/test_telemetry.rs \
        tests/test_cache.rs tests/test_embeddings.rs
git commit -m "feat: add load balancer with round-robin and weighted strategies"
```
