# Phase 14: Complete Gap Analysis Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement all remaining items from the LiteLLM feature gap analysis (`docs/2026-04-02-litellm-feature-gap.md`) that were not addressed in Phases 1–13.

**Architecture:** Group A items (items #19, #22, #24, #25, #29–#33) are fully-specified tasks with TDD steps — they extend existing patterns without requiring new subsystems. Group B items (#23, #26–#28, #34–#40) are sub-project stubs with scope, interface, and integration notes; each is large enough to be a standalone phase.

**Tech Stack:** Rust, axum, sqlx (SQLite + PostgreSQL), tokio, serde_json, reqwest, DashMap; optional: `aws-sdk-s3` for cold storage, `arc-swap` crate for hot-reload, `rand` crate for retry jitter

---

## Critical Codebase Patterns (read before implementing any task)

### Cost Logging Pattern
Cost logging always requires TWO steps: (1) create a `Prompt` record, (2) create a `CostLedgerEntry` referencing the prompt's ID. This is always fire-and-forget via `tokio::spawn`.

```rust
// Always done inside tokio::spawn:
let prompt = NewPrompt { user_id, session_id: None, request_model: model.clone(),
    routed_model: canonical_model.clone(), provider: provider_name.clone(),
    messages: messages_json, response: Some(response_text), finish_reason: Some(finish),
    prompt_tokens: prompt_tokens as i64, completion_tokens: completion_tokens as i64,
    cost_usd: cost, latency_ms: Some(latency_ms), tags: "[]".to_string(), project: None };
match PromptRepository::create(&*db, prompt).await {
    Ok(saved_prompt) => {
        let ledger = NewCostLedgerEntry {
            user_id, prompt_id: saved_prompt.id, model: canonical_model.clone(),
            provider: provider_name, project: None,
            tokens_in: prompt_tokens as i64, tokens_out: completion_tokens as i64,
            cost_usd: cost, api_key_id,
        };
        let _ = CostRepository::create(&*db, ledger).await;
    }
    Err(e) => tracing::error!("Failed to record prompt: {}", e),
}
```

### Policy Check Signature
```rust
// Takes user AND model — both required
state.policy.check(&user, &model).await?
```

### AppState Test Files
Any new field added to `AppState` must be added to ALL 8 of these test files:
1. `tests/test_completions.rs`
2. `tests/test_cache.rs`
3. `tests/test_embeddings.rs`
4. `tests/test_messages.rs`
5. `tests/test_per_key_budgets.rs`
6. `tests/test_dashboard.rs`
7. `tests/test_prometheus.rs`
8. `tests/test_telemetry.rs` (otel-gated, check `#[cfg(feature = "otel")]`)

### CostLedgerEntry Actual Struct
```rust
pub struct CostLedgerEntry {
    pub id: i64, pub user_id: i64, pub prompt_id: i64,
    pub model: String, pub provider: String, pub project: Option<String>,
    pub tokens_in: i64, pub tokens_out: i64, pub cost_usd: f64,
    pub created_at: String,
    #[sqlx(default)] pub api_key_id: Option<i64>,
}
```

---

## Status Reference: What Was Done in Prior Phases

| Gap # | Feature | Phase |
|-------|---------|-------|
| 1 | `/v1/messages` Anthropic passthrough | Phase 11 |
| 2 | Enforced token limits (TPM/RPM) | Phase 11+ (policy.rs checks `limit_tokens` at line 150) |
| 3 | Custom pricing tables | Phase 11 |
| 4 | Fallback chain retry | Phase 11 |
| 5 | Prometheus `/metrics` | Phase 11d |
| 6 | Complexity router | Phase 11 |
| 7 | Response caching (exact match) | Phase 11 |
| 8 | Embeddings endpoint | Phase 11 |
| 9 | Per-key budgets | Phase 12 |
| 10 | Azure OpenAI adapter | Phase 11 |
| 11 | AWS Bedrock adapter | Phase 11 |
| 12 | Load balancing | Phase 11 |
| 13 | Groq/Mistral/DeepSeek adapters | Phase 11 |
| 14 | Circuit breaker | Phase 13 |
| 15 | IP rate limiting | Phase 13 |
| 16 | Concurrent request limits | Phase 12 |
| 17 | Spend reset API | Phase 12 |
| 18 | Per-tag budgets | Phase 13 |
| 20 | Anthropic cache_control passthrough | Phase 12 |
| 21 | Key expiration | Phase 12 |

**Remaining:** #19, #22–#40

---

## Group A: Fully-Specified Tasks

---

### Task 1: Cold Storage / Log Archival (Gap #19)

Archive old `cost_log` rows to an S3-compatible bucket to keep SQLite size manageable. Rows older than a configurable threshold are written to NDJSON in S3 and deleted from the database.

**Files:**
- Create: `src/archival/mod.rs`
- Create: `src/archival/s3.rs`
- Modify: `src/config/schema.rs` — add `ArchivalConfig`
- Modify: `src/db/repositories/costs.rs` — add `list_cost_entries_before` and `delete_cost_entries_by_ids`
- Modify: `src/db/sqlite/costs.rs` — implement new trait methods
- Modify: `src/cli/mod.rs` — spawn archival background task when `archival.enabled`
- Modify: `Cargo.toml` — add `s3-archival` feature flag
- Test: `tests/test_archival.rs`

**Pitfalls:**
- `CostLedgerEntry` fields are: `id, user_id, prompt_id, model, provider, project, tokens_in, tokens_out, cost_usd, created_at, api_key_id`. NOT `prompt_tokens`/`completion_tokens`.
- The archival job must DELETE only after a successful S3 PUT, never before.
- `CostRepository` new methods must be added to the trait in `src/db/repositories/costs.rs` AND to the `DatabaseProvider` supertrait aggregate in `src/api/app.rs` (since archival calls `self.db.list_cost_entries_before()` via `Arc<dyn DatabaseProvider>`).
- Feature-gate with `s3-archival` feature in `Cargo.toml` to avoid pulling AWS deps for users who don't need it.

- [ ] **Step 1: Add ArchivalConfig to `src/config/schema.rs`**

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchivalConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Archive cost_log rows older than this many days
    #[serde(default = "default_archive_after_days")]
    pub after_days: u32,
    /// S3-compatible endpoint (e.g. "https://s3.amazonaws.com" or MinIO URL)
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default = "default_archive_prefix")]
    pub prefix: String,
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default = "default_archive_region")]
    pub region: String,
}

impl Default for ArchivalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_days: default_archive_after_days(),
            endpoint: String::new(),
            bucket: String::new(),
            prefix: default_archive_prefix(),
            access_key: String::new(),
            secret_key: String::new(),
            region: default_archive_region(),
        }
    }
}

fn default_archive_after_days() -> u32 { 90 }
fn default_archive_prefix() -> String { "modelrouter/cost-logs".to_string() }
fn default_archive_region() -> String { "us-east-1".to_string() }
```

Add to `Settings`:
```rust
#[serde(default)]
pub archival: ArchivalConfig,
```

- [ ] **Step 2: Run `cargo build` to confirm schema compiles**

Run: `cargo build`
Expected: compiles with no errors

- [ ] **Step 3: Add new methods to `CostRepository` trait in `src/db/repositories/costs.rs`**

```rust
async fn list_cost_entries_before(&self, cutoff: &str) -> anyhow::Result<Vec<crate::db::models::CostLedgerEntry>>;
async fn delete_cost_entries_by_ids(&self, ids: &[i64]) -> anyhow::Result<()>;
```

Note: Since `DatabaseProvider` in `src/api/app.rs` is a supertrait that requires `CostRepository`, these methods automatically become available on `Arc<dyn DatabaseProvider>` — no changes needed to `app.rs`.

- [ ] **Step 4: Implement in `src/db/sqlite/costs.rs`**

```rust
async fn list_cost_entries_before(&self, cutoff: &str) -> anyhow::Result<Vec<CostLedgerEntry>> {
    let rows = sqlx::query_as!(
        CostLedgerEntry,
        r#"SELECT id, user_id, prompt_id, model, provider, project,
                  tokens_in, tokens_out, cost_usd, created_at, api_key_id
           FROM cost_log WHERE created_at < ? ORDER BY created_at ASC LIMIT 10000"#,
        cutoff
    )
    .fetch_all(&self.pool)
    .await?;
    Ok(rows)
}

async fn delete_cost_entries_by_ids(&self, ids: &[i64]) -> anyhow::Result<()> {
    for chunk in ids.chunks(500) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM cost_log WHERE id IN ({})", placeholders);
        let mut q = sqlx::query(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        q.execute(&self.pool).await?;
    }
    Ok(())
}
```

Also add stub implementations to `src/db/postgres/costs.rs` to keep the postgres build compiling:
```rust
async fn list_cost_entries_before(&self, _cutoff: &str) -> anyhow::Result<Vec<CostLedgerEntry>> {
    Ok(vec![])
}
async fn delete_cost_entries_by_ids(&self, _ids: &[i64]) -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 5: Write failing test**

Create `tests/test_archival.rs`:

```rust
mod common;

use modelrouter::archival::rows_to_ndjson;
use modelrouter::db::models::CostLedgerEntry;

#[test]
fn rows_to_ndjson_produces_one_line_per_row() {
    let rows = vec![
        CostLedgerEntry {
            id: 1,
            user_id: 42,
            prompt_id: 7,
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.01,
            created_at: "2024-01-01T00:00:00+00:00".to_string(),
            api_key_id: None,
        },
        CostLedgerEntry {
            id: 2,
            user_id: 42,
            prompt_id: 8,
            model: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            project: None,
            tokens_in: 200,
            tokens_out: 100,
            cost_usd: 0.02,
            created_at: "2024-01-02T00:00:00+00:00".to_string(),
            api_key_id: None,
        },
    ];
    let ndjson = rows_to_ndjson(&rows);
    let lines: Vec<&str> = ndjson.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"id\":1"));
    assert!(lines[1].contains("\"id\":2"));
    assert!(lines[0].contains("gpt-4o"));
}
```

Run: `cargo test test_archival`
Expected: compile error (archival module doesn't exist)

- [ ] **Step 6: Create `src/archival/mod.rs` and `src/archival/s3.rs`**

`src/archival/mod.rs`:
```rust
#[cfg(feature = "s3-archival")]
pub mod s3;

#[cfg(feature = "s3-archival")]
pub use s3::{ArchivalJob, spawn_archival_task};

use crate::db::models::CostLedgerEntry;

/// Serialize rows to NDJSON (one JSON object per line)
pub fn rows_to_ndjson(rows: &[CostLedgerEntry]) -> String {
    rows.iter()
        .map(|r| serde_json::to_string(r).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}
```

`src/archival/s3.rs`:
```rust
use crate::config::schema::ArchivalConfig;
use crate::db::repositories::costs::CostRepository;
use crate::api::app::DatabaseProvider;
use std::sync::Arc;

pub struct ArchivalJob {
    config: ArchivalConfig,
    db: Arc<dyn DatabaseProvider>,
}

impl ArchivalJob {
    pub fn new(config: ArchivalConfig, db: Arc<dyn DatabaseProvider>) -> Self {
        Self { config, db }
    }

    pub async fn run_once(&self) -> anyhow::Result<usize> {
        use chrono::{Duration, Utc};

        let cutoff = Utc::now() - Duration::days(self.config.after_days as i64);
        let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();

        let rows = self.db.list_cost_entries_before(&cutoff_str).await?;
        if rows.is_empty() {
            return Ok(0);
        }

        let ndjson = super::rows_to_ndjson(&rows);
        let object_key = format!(
            "{}/{}.ndjson",
            self.config.prefix,
            cutoff.format("%Y-%m-%d")
        );
        self.upload_ndjson(&object_key, ndjson).await?;

        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        self.db.delete_cost_entries_by_ids(&ids).await?;

        Ok(ids.len())
    }

    async fn upload_ndjson(&self, key: &str, content: String) -> anyhow::Result<()> {
        let url = format!("{}/{}/{}", self.config.endpoint, self.config.bucket, key);
        let client = reqwest::Client::new();
        let resp = client
            .put(&url)
            .header("Content-Type", "application/x-ndjson")
            .body(content)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("S3 upload failed: {}", resp.status());
        }
        Ok(())
    }
}

pub fn spawn_archival_task(job: ArchivalJob) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = job.run_once().await {
                tracing::warn!("archival job failed: {e}");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(6 * 3600)).await;
        }
    });
}
```

- [ ] **Step 7: Add `s3-archival` feature to `Cargo.toml`**

```toml
[features]
s3-archival = []
```

- [ ] **Step 8: Wire archival task in `src/cli/mod.rs`**

After the server is configured, before `serve`:
```rust
#[cfg(feature = "s3-archival")]
if settings.archival.enabled {
    let job = modelrouter::archival::ArchivalJob::new(settings.archival.clone(), db_provider.clone());
    modelrouter::archival::spawn_archival_task(job);
}
```

- [ ] **Step 9: Run tests**

Run: `cargo test`
Expected: all tests pass

Run: `cargo build --features s3-archival`
Expected: compiles without error

- [ ] **Step 10: Commit**

```bash
git add src/archival/ src/config/schema.rs src/db/repositories/costs.rs src/db/sqlite/costs.rs src/db/postgres/costs.rs src/cli/mod.rs Cargo.toml tests/test_archival.rs
git commit -m "feat: cold storage archival of old cost_log rows to S3-compatible endpoint"
```

---

### Task 2: Config Hot-Reload for Model Deployments (Gap #22)

Background task that re-reads the config file every 30 seconds and replaces `AppState.settings` without a restart.

**Files:**
- Create: `src/config/loader.rs`
- Modify: `src/config/mod.rs` — expose `load_from_path`
- Modify: `src/api/app.rs` — add `live_settings: Arc<ArcSwap<Settings>>`
- Modify: `src/cli/mod.rs` — spawn hot-reload task
- Modify: `Cargo.toml` — add `arc-swap = "1"`
- Modify: all 8 AppState test files — add `live_settings` field
- Test: `tests/test_hot_reload.rs`

**Pitfalls:**
- `AppState` derives `Clone`. `Arc<ArcSwap<Settings>>` IS Clone (Arc is always Clone), so this is fine.
- All 8 AppState test constructors must be updated — failing to do this causes compile errors in the entire test suite.
- Handlers that need the most-current settings should read via `state.live_settings.load()`. Handlers can keep reading `state.settings` for backward compat; they'll just get the startup-time config.
- `tempfile` crate needed for the test — add to `[dev-dependencies]` if not already present.

- [ ] **Step 1: Add `arc-swap` to `Cargo.toml`**

```toml
[dependencies]
arc-swap = "1"
```

Check if `tempfile` is already a dev-dependency. If not, add:
```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write failing test**

Create `tests/test_hot_reload.rs`:
```rust
mod common;

use modelrouter::config::loader::SettingsLoader;

#[test]
fn settings_loader_reload_returns_updated_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[server]\nport = 8080\n").unwrap();

    let loader = SettingsLoader::new(path.to_str().unwrap().to_string());
    let s1 = loader.load().unwrap();
    assert_eq!(s1.server.port, 8080);

    std::fs::write(&path, "[server]\nport = 9090\n").unwrap();
    let s2 = loader.load().unwrap();
    assert_eq!(s2.server.port, 9090);
}
```

Run: `cargo test test_hot_reload`
Expected: compile error (loader module doesn't exist)

- [ ] **Step 3: Create `src/config/loader.rs`**

```rust
use super::Settings;

pub struct SettingsLoader {
    config_path: String,
}

impl SettingsLoader {
    pub fn new(config_path: String) -> Self {
        Self { config_path }
    }

    pub fn load(&self) -> anyhow::Result<Settings> {
        super::load_from_path(&self.config_path)
    }
}
```

- [ ] **Step 4: Expose `load_from_path` in `src/config/mod.rs`**

Add or expose the existing config loading logic as a public function:
```rust
pub mod loader;

pub fn load_from_path(path: &str) -> anyhow::Result<Settings> {
    // delegate to the existing config load mechanism (config-rs or toml::from_str)
}
```

Look at the current `mod.rs` to find how config is loaded today and wrap it.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test test_hot_reload`
Expected: PASS

- [ ] **Step 6: Add `live_settings` to `AppState` in `src/api/app.rs`**

```rust
use arc_swap::ArcSwap;

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    /// Hot-reloadable settings pointer. Use .load() to get the current value.
    pub live_settings: Arc<ArcSwap<Settings>>,
    // ... all existing fields unchanged
}
```

- [ ] **Step 7: Update all 8 AppState test files**

In each of the 8 test files listed in the Critical Patterns section above, add the `live_settings` field to the `AppState { ... }` struct literal:
```rust
live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
```

Add `use arc_swap;` or import via the full path if needed.

- [ ] **Step 8: Update `src/cli/mod.rs` to initialise `live_settings` and spawn hot-reload task**

```rust
use arc_swap::ArcSwap;
use std::sync::Arc;

// In AppState construction:
let live_settings = Arc::new(ArcSwap::from_pointee((*settings).clone()));

// Add to AppState { ... }:
live_settings: live_settings.clone(),

// After serve setup, if config_path is known:
if let Some(ref config_path) = resolved_config_path {
    let loader = modelrouter::config::loader::SettingsLoader::new(config_path.clone());
    let live = live_settings.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            match loader.load() {
                Ok(new_settings) => {
                    live.store(Arc::new(new_settings));
                    tracing::info!("config hot-reloaded");
                }
                Err(e) => tracing::warn!("config reload failed: {e}"),
            }
        }
    });
}
```

- [ ] **Step 9: Run tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 10: Commit**

```bash
git add src/config/loader.rs src/config/mod.rs src/api/app.rs src/cli/mod.rs Cargo.toml tests/test_hot_reload.rs tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: config hot-reload via arc-swap, re-reads config every 30s without restart"
```

---

### Task 3: LangFuse / LangSmith Callback (Gap #24)

After each successful completion, POST a structured event to a configured LangFuse or LangSmith endpoint as a fire-and-forget background task. Failures are logged but never surface to the caller.

**Files:**
- Create: `src/callbacks/mod.rs`
- Create: `src/callbacks/langfuse.rs`
- Create: `src/callbacks/langsmith.rs`
- Modify: `src/config/schema.rs` — add `CallbacksConfig`
- Modify: `src/api/app.rs` — add `callbacks: Arc<CallbackDispatcher>`
- Modify: `src/api/routes/completions.rs` — dispatch callback in the existing `tokio::spawn` block after cost log
- Modify: `src/api/routes/messages.rs` — same
- Modify: all 8 AppState test files — add `callbacks` field
- Test: `tests/test_callbacks.rs`

**Pitfalls:**
- The callback MUST be inside an existing or new `tokio::spawn` — never awaited in the request path.
- `CallbackBackend::send` is `fn` not `async fn` and calls `tokio::spawn` internally. This is the correct pattern for fire-and-forget.
- All 8 AppState test files must add `callbacks: Arc::new(crate::callbacks::CallbackDispatcher::new(vec![]))`.

- [ ] **Step 1: Add `CallbacksConfig` to `src/config/schema.rs`**

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CallbacksConfig {
    #[serde(default)]
    pub langfuse: Option<LangFuseConfig>,
    #[serde(default)]
    pub langsmith: Option<LangSmithConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LangFuseConfig {
    pub public_key: String,
    pub secret_key: String,
    #[serde(default = "default_langfuse_host")]
    pub host: String,
}

fn default_langfuse_host() -> String { "https://cloud.langfuse.com".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LangSmithConfig {
    pub api_key: String,
    #[serde(default = "default_langsmith_host")]
    pub host: String,
    pub project: String,
}

fn default_langsmith_host() -> String { "https://api.smith.langchain.com".to_string() }
```

Add to `Settings`:
```rust
#[serde(default)]
pub callbacks: CallbacksConfig,
```

- [ ] **Step 2: Write failing test**

Create `tests/test_callbacks.rs`:
```rust
mod common;

use modelrouter::callbacks::{CallbackDispatcher, CallbackEvent};

#[tokio::test]
async fn dispatcher_with_no_backends_is_a_no_op() {
    let dispatcher = CallbackDispatcher::new(vec![]);
    // Must not panic
    dispatcher.dispatch(CallbackEvent {
        trace_id: "test-id".to_string(),
        user_id: 1,
        model: "gpt-4o".to_string(),
        provider: "openai".to_string(),
        input: serde_json::json!([{"role": "user", "content": "Hello"}]),
        output: "Hello back".to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_usd: 0.001,
        latency_ms: 200,
    });
}
```

Run: `cargo test test_callbacks`
Expected: compile error

- [ ] **Step 3: Create `src/callbacks/mod.rs`**

```rust
pub mod langfuse;
pub mod langsmith;

use serde_json::Value;

#[derive(Clone)]
pub struct CallbackEvent {
    pub trace_id: String,
    pub user_id: i64,
    pub model: String,
    pub provider: String,
    pub input: Value,
    pub output: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cost_usd: f64,
    pub latency_ms: i64,
}

pub trait CallbackBackend: Send + Sync {
    fn send(&self, event: CallbackEvent);
}

pub struct CallbackDispatcher {
    backends: Vec<Box<dyn CallbackBackend>>,
}

impl CallbackDispatcher {
    pub fn new(backends: Vec<Box<dyn CallbackBackend>>) -> Self {
        Self { backends }
    }

    pub fn dispatch(&self, event: CallbackEvent) {
        for backend in &self.backends {
            backend.send(event.clone());
        }
    }
}
```

- [ ] **Step 4: Create `src/callbacks/langfuse.rs`**

```rust
use super::{CallbackBackend, CallbackEvent};
use crate::config::schema::LangFuseConfig;

pub struct LangFuseBackend {
    config: LangFuseConfig,
    client: reqwest::Client,
}

impl LangFuseBackend {
    pub fn new(config: LangFuseConfig) -> Self {
        Self { config, client: reqwest::Client::new() }
    }
}

impl CallbackBackend for LangFuseBackend {
    fn send(&self, event: CallbackEvent) {
        let url = format!("{}/api/public/traces", self.config.host);
        let public_key = self.config.public_key.clone();
        let secret_key = self.config.secret_key.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({
                "id": event.trace_id,
                "name": "modelrouter.completion",
                "input": event.input,
                "output": event.output,
                "metadata": {
                    "model": event.model,
                    "provider": event.provider,
                    "prompt_tokens": event.prompt_tokens,
                    "completion_tokens": event.completion_tokens,
                    "cost_usd": event.cost_usd,
                    "latency_ms": event.latency_ms,
                    "user_id": event.user_id,
                }
            });
            if let Err(e) = client
                .post(&url)
                .basic_auth(&public_key, Some(&secret_key))
                .json(&body)
                .send()
                .await
            {
                tracing::warn!("langfuse callback failed: {e}");
            }
        });
    }
}
```

- [ ] **Step 5: Create `src/callbacks/langsmith.rs`**

```rust
use super::{CallbackBackend, CallbackEvent};
use crate::config::schema::LangSmithConfig;

pub struct LangSmithBackend {
    config: LangSmithConfig,
    client: reqwest::Client,
}

impl LangSmithBackend {
    pub fn new(config: LangSmithConfig) -> Self {
        Self { config, client: reqwest::Client::new() }
    }
}

impl CallbackBackend for LangSmithBackend {
    fn send(&self, event: CallbackEvent) {
        let url = format!("{}/runs", self.config.host);
        let api_key = self.config.api_key.clone();
        let project = self.config.project.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let body = serde_json::json!({
                "id": event.trace_id,
                "name": "modelrouter.completion",
                "run_type": "llm",
                "inputs": { "messages": event.input },
                "outputs": { "content": event.output },
                "extra": {
                    "model": event.model,
                    "provider": event.provider,
                    "prompt_tokens": event.prompt_tokens,
                    "completion_tokens": event.completion_tokens,
                    "cost_usd": event.cost_usd,
                    "latency_ms": event.latency_ms,
                    "session_name": project,
                }
            });
            if let Err(e) = client
                .post(&url)
                .header("x-api-key", &api_key)
                .json(&body)
                .send()
                .await
            {
                tracing::warn!("langsmith callback failed: {e}");
            }
        });
    }
}
```

- [ ] **Step 6: Add `callbacks` field to `AppState` in `src/api/app.rs`**

```rust
pub callbacks: Arc<crate::callbacks::CallbackDispatcher>,
```

- [ ] **Step 7: Update all 8 AppState test files**

In each test file's `AppState { ... }` construction block, add:
```rust
callbacks: Arc::new(crate::callbacks::CallbackDispatcher::new(vec![])),
```

- [ ] **Step 8: Build dispatcher in `src/cli/mod.rs`**

```rust
let mut cb_backends: Vec<Box<dyn crate::callbacks::CallbackBackend>> = vec![];
if let Some(lf_config) = settings.callbacks.langfuse.clone() {
    cb_backends.push(Box::new(crate::callbacks::langfuse::LangFuseBackend::new(lf_config)));
}
if let Some(ls_config) = settings.callbacks.langsmith.clone() {
    cb_backends.push(Box::new(crate::callbacks::langsmith::LangSmithBackend::new(ls_config)));
}
let callbacks = Arc::new(crate::callbacks::CallbackDispatcher::new(cb_backends));
```

Add `callbacks` to the `AppState { ... }` construction.

- [ ] **Step 9: Dispatch callback in `src/api/routes/completions.rs`**

Inside the existing `tokio::spawn` block, after the `CostRepository::create` call:
```rust
state_clone.callbacks.dispatch(crate::callbacks::CallbackEvent {
    trace_id: format!("{}", saved_prompt.id), // use prompt ID as trace ID
    user_id,
    model: canonical_clone.clone(),
    provider: provider_clone.clone(),
    input: serde_json::from_str(&messages_json).unwrap_or(serde_json::Value::Null),
    output: response_clone.clone(),
    prompt_tokens,
    completion_tokens,
    cost_usd: cost,
    latency_ms,
});
```

Do the same in `src/api/routes/messages.rs` inside its logging spawn block.

- [ ] **Step 10: Run tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 11: Commit**

```bash
git add src/callbacks/ src/config/schema.rs src/api/app.rs src/cli/mod.rs src/api/routes/completions.rs src/api/routes/messages.rs tests/test_callbacks.rs tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: LangFuse and LangSmith fire-and-forget callback backends"
```

---

### Task 4: Session-Based Rate Limits (Gap #25)

Limit tokens-per-minute (TPM) and requests-per-minute (RPM) scoped to a `session_id` passed in the request body. Prevents a single runaway agent loop from exhausting a user's budget.

**Files:**
- Create: `src/router/session_limits.rs`
- Modify: `src/config/schema.rs` — add `SessionLimitConfig`
- Modify: `src/api/app.rs` — add `session_limiter`
- Modify: `src/cli/mod.rs` — construct limiter
- Modify: `src/api/routes/completions.rs` — check session limits
- Modify: all 8 AppState test files
- Test: `tests/test_session_limits.rs`

**Pitfalls:**
- Session ID comes from `body["session_id"].as_str()`. If absent, skip the check entirely.
- Use in-memory `DashMap` — no DB persistence. Buckets are scoped to the current minute.
- The session limit check runs AFTER policy check and BEFORE provider dispatch.
- All 8 AppState test files need `session_limiter: Arc::new(crate::router::session_limits::SessionLimiter::new(0, 0))`.

- [ ] **Step 1: Add `SessionLimitConfig` to `src/config/schema.rs`**

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SessionLimitConfig {
    /// Max tokens per minute per session. 0 = disabled.
    #[serde(default)]
    pub tpm: u32,
    /// Max requests per minute per session. 0 = disabled.
    #[serde(default)]
    pub rpm: u32,
}
```

Add to `Settings`:
```rust
#[serde(default)]
pub session_limits: SessionLimitConfig,
```

- [ ] **Step 2: Write failing test**

Create `tests/test_session_limits.rs`:
```rust
use modelrouter::router::session_limits::SessionLimiter;

#[test]
fn session_limiter_allows_first_request() {
    let limiter = SessionLimiter::new(10000, 10);
    assert!(limiter.check_and_record("session-abc", 100));
}

#[test]
fn session_limiter_blocks_after_rpm_exceeded() {
    let limiter = SessionLimiter::new(10000, 2);
    assert!(limiter.check_and_record("session-abc", 10));
    assert!(limiter.check_and_record("session-abc", 10));
    assert!(!limiter.check_and_record("session-abc", 10)); // 3rd blocked
}

#[test]
fn session_limiter_blocks_after_tpm_exceeded() {
    let limiter = SessionLimiter::new(50, 1000);
    assert!(limiter.check_and_record("session-abc", 40));
    assert!(!limiter.check_and_record("session-abc", 20)); // 40+20 > 50
}

#[test]
fn session_limiter_different_sessions_are_independent() {
    let limiter = SessionLimiter::new(10000, 1);
    assert!(limiter.check_and_record("session-a", 10));
    assert!(limiter.check_and_record("session-b", 10));
}

#[test]
fn session_limiter_zero_limits_always_allows() {
    let limiter = SessionLimiter::new(0, 0);
    for _ in 0..1000 {
        assert!(limiter.check_and_record("session-x", 999999));
    }
}
```

Run: `cargo test test_session_limits`
Expected: compile error

- [ ] **Step 3: Create `src/router/session_limits.rs`**

```rust
use dashmap::DashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct SessionBucket {
    window_key: String,
    request_count: u32,
    token_count: u32,
}

pub struct SessionLimiter {
    tpm: u32,
    rpm: u32,
    buckets: DashMap<String, Mutex<SessionBucket>>,
}

impl SessionLimiter {
    pub fn new(tpm: u32, rpm: u32) -> Self {
        Self { tpm, rpm, buckets: DashMap::new() }
    }

    fn current_minute_key() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        (secs / 60).to_string()
    }

    /// Returns true if request is allowed; records tokens if so.
    pub fn check_and_record(&self, session_id: &str, tokens: u32) -> bool {
        if self.tpm == 0 && self.rpm == 0 {
            return true;
        }
        let window = Self::current_minute_key();
        let entry = self.buckets
            .entry(session_id.to_string())
            .or_insert_with(|| Mutex::new(SessionBucket {
                window_key: window.clone(),
                request_count: 0,
                token_count: 0,
            }));
        let mut bucket = entry.lock().unwrap();
        if bucket.window_key != window {
            bucket.window_key = window;
            bucket.request_count = 0;
            bucket.token_count = 0;
        }
        if self.rpm > 0 && bucket.request_count >= self.rpm {
            return false;
        }
        if self.tpm > 0 && bucket.token_count + tokens > self.tpm {
            return false;
        }
        bucket.request_count += 1;
        bucket.token_count += tokens;
        true
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_session_limits`
Expected: PASS

- [ ] **Step 5: Add `session_limiter` to `AppState`**

In `src/api/app.rs`:
```rust
pub session_limiter: Arc<crate::router::session_limits::SessionLimiter>,
```

In `src/cli/mod.rs`:
```rust
let session_limiter = Arc::new(crate::router::session_limits::SessionLimiter::new(
    settings.session_limits.tpm,
    settings.session_limits.rpm,
));
```

- [ ] **Step 6: Update all 8 AppState test files**

Add to each AppState construction:
```rust
session_limiter: Arc::new(crate::router::session_limits::SessionLimiter::new(0, 0)),
```

- [ ] **Step 7: Check session limits in `src/api/routes/completions.rs`**

After the policy check block, before the provider dispatch:
```rust
if let Some(session_id) = body["session_id"].as_str() {
    let estimated_tokens = body["messages"]
        .as_array()
        .map(|m| m.iter().map(|msg| {
            msg["content"].as_str().map(|s| (s.len() / 4) as u32).unwrap_or(50)
        }).sum::<u32>())
        .unwrap_or(100);
    if !state.session_limiter.check_and_record(session_id, estimated_tokens) {
        return Err(ApiError::RateLimitExceeded("session rate limit exceeded".to_string()));
    }
}
```

- [ ] **Step 8: Run tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 9: Commit**

```bash
git add src/router/session_limits.rs src/config/schema.rs src/api/app.rs src/cli/mod.rs src/api/routes/completions.rs tests/test_session_limits.rs tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: per-session TPM/RPM rate limits for agent loop protection"
```

---

### Task 5: OpenAI Responses API Passthrough (Gap #29)

Add `POST /v1/responses` as a passthrough route with auth, policy check, and cost logging.

**Files:**
- Create: `src/api/routes/responses.rs`
- Modify: `src/api/routes/mod.rs`
- Modify: `src/api/app.rs` — register route
- Test: `tests/test_responses.rs`

**Pitfalls:**
- Cost logging pattern: create `NewPrompt` first, then `NewCostLedgerEntry` with `prompt_id: saved_prompt.id`. See the "Critical Codebase Patterns" section above.
- `policy.check(&user, &model)` — two arguments.
- For test support, follow the same pattern as `test_completions.rs` — the mock adapter's `complete()` method handles the translated body.

- [ ] **Step 1: Write failing test**

Create `tests/test_responses.rs`:
```rust
mod common;

#[tokio::test]
async fn responses_unauthenticated_returns_401() {
    let server = common::test_app().await;
    let resp = server
        .post("/v1/responses")
        .json(&serde_json::json!({"model": "gpt-4o", "input": "Hello"}))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn responses_authenticated_returns_200() {
    let server = common::test_app().await;
    let resp = server
        .post("/v1/responses")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({"model": "gpt-4o", "input": "Hello"}))
        .await;
    assert_eq!(resp.status_code(), 200);
}
```

Look at `tests/common/mod.rs` to confirm the function name is `test_app()` or `test_app_with_mock()`.

Run: `cargo test test_responses`
Expected: fail with 404

- [ ] **Step 2: Create `src/api/routes/responses.rs`**

```rust
use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use tracing::Instrument;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::policy::PolicyDecision,
};

pub async fn responses_handler(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let span = tracing::info_span!("responses", user_id = tracing::field::Empty, model = tracing::field::Empty);
    responses_inner(State(state), user, Json(body)).instrument(span).await
}

async fn responses_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    tracing::Span::current().record("user_id", user.id);

    let model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    tracing::Span::current().record("model", &model);

    // Policy check — both user AND model required
    match state.policy.check(&user, &model).await? {
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
        PolicyDecision::Allow { .. } => {}
    }

    let (provider_name, canonical_model) = state.router.resolve(&model, &state.settings);
    let provider = state.provider_registry.get(&provider_name)
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.clone()))?;

    // Translate Responses API `input` to messages array if needed
    let mut chat_body = body.clone();
    if chat_body.get("messages").is_none() || chat_body["messages"].is_null() {
        if let Some(input) = body["input"].as_str() {
            chat_body["messages"] = serde_json::json!([{"role": "user", "content": input}]);
        }
    }
    // Remove Responses-API-specific fields that confuse chat completions
    if let Some(obj) = chat_body.as_object_mut() {
        obj.remove("input");
    }

    let start = std::time::Instant::now();
    let result = provider.complete(&canonical_model, &chat_body).await
        .map_err(|e| ApiError::ProviderError(e.to_string()))?;
    let latency_ms = start.elapsed().as_millis() as i64;

    let prompt_tokens = result.prompt_tokens;
    let completion_tokens = result.completion_tokens;
    let cost = state.cost_calc.calculate(&canonical_model, prompt_tokens, completion_tokens);

    // Fire-and-forget cost logging
    let state_c = state.clone();
    let canonical_c = canonical_model.clone();
    let provider_c = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    let messages_json = serde_json::to_string(&chat_body["messages"]).unwrap_or_default();
    let response_text = result.content.clone();
    let finish = result.finish_reason.clone();
    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id, session_id: None,
            request_model: model.clone(), routed_model: canonical_c.clone(),
            provider: provider_c.clone(), messages: messages_json,
            response: Some(response_text), finish_reason: Some(finish),
            prompt_tokens: prompt_tokens as i64, completion_tokens: completion_tokens as i64,
            cost_usd: cost, latency_ms: Some(latency_ms),
            tags: "[]".to_string(), project: None,
        };
        match PromptRepository::create(&*state_c.db, prompt).await {
            Ok(saved_prompt) => {
                let ledger = NewCostLedgerEntry {
                    user_id, prompt_id: saved_prompt.id,
                    model: canonical_c, provider: provider_c, project: None,
                    tokens_in: prompt_tokens as i64, tokens_out: completion_tokens as i64,
                    cost_usd: cost, api_key_id,
                };
                if let Err(e) = CostRepository::create(&*state_c.db, ledger).await {
                    tracing::error!("failed to record responses cost: {e}");
                }
            }
            Err(e) => tracing::error!("failed to record responses prompt: {e}"),
        }
    });

    // Return OpenAI-style response (chat completions format)
    Ok(axum::Json(serde_json::json!({
        "id": format!("resp_{}", result.prompt_tokens),
        "object": "response",
        "model": canonical_model,
        "choices": [{
            "message": { "role": "assistant", "content": result.content },
            "finish_reason": result.finish_reason,
        }],
        "usage": { "input_tokens": result.prompt_tokens, "output_tokens": result.completion_tokens }
    })).into_response())
}
```

Look at the `ProviderAdapter` trait to check what `provider.complete()` returns (a `CompletionResult` or `Value`?) and adjust the result access accordingly. Match the pattern used in `completions.rs`.

- [ ] **Step 3: Register route in `src/api/app.rs`**

```rust
use crate::api::routes::responses::responses_handler;
// in build_router:
.route("/v1/responses", post(responses_handler))
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add src/api/routes/responses.rs src/api/routes/mod.rs src/api/app.rs tests/test_responses.rs
git commit -m "feat: /v1/responses passthrough with auth and cost logging"
```

---

### Task 6: Image Generation Endpoint (Gap #32)

Add `POST /v1/images/generations` via a new `OpenAIImageAdapter`. Cost is per-image (not per-token).

**Files:**
- Create: `src/providers/openai_images.rs`
- Create: `src/api/routes/images.rs`
- Modify: `src/api/routes/mod.rs`
- Modify: `src/api/app.rs` — register route
- Modify: `src/providers/mod.rs` — declare module
- Test: `tests/test_images.rs`

**Pitfalls:**
- Images handler calls `OpenAIImageAdapter` directly (not through the `ProviderRegistry`) because the registry is typed to the chat completions adapter trait.
- Cost logging for images: create a `NewPrompt` with `prompt_tokens: n_images as i64, completion_tokens: 0` and `NewCostLedgerEntry` with the same values. Use `tokens_in: n_images as i64` to record image count.
- `policy.check(&user, &model)` — don't forget the model argument.
- For tests, the `OpenAIImageAdapter` makes real HTTP calls. To make tests pass without a real API key, configure the adapter to use `api_base` from provider config and in tests, point it to an invalid/mock URL that returns 200. Alternatively: make the test only check auth (401 case) and rely on integration tests for the 200 case.

- [ ] **Step 1: Write failing test**

Create `tests/test_images.rs`:
```rust
mod common;

#[tokio::test]
async fn image_generation_unauthenticated_returns_401() {
    let server = common::test_app().await;
    let resp = server
        .post("/v1/images/generations")
        .json(&serde_json::json!({"model": "dall-e-3", "prompt": "a cat", "n": 1}))
        .await;
    assert_eq!(resp.status_code(), 401);
}
```

Run: `cargo test test_images`
Expected: fail (404 not 401, route doesn't exist)

- [ ] **Step 2: Create `src/providers/openai_images.rs`**

```rust
use serde_json::Value;
use crate::config::schema::ProviderConfig;

pub struct OpenAIImageAdapter {
    api_key: String,
    api_base: String,
    client: reqwest::Client,
}

impl OpenAIImageAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        Self {
            api_key: config.api_key.clone(),
            api_base: config.api_base.clone()
                .unwrap_or_else(|| "https://api.openai.com".to_string()),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(config.timeout_secs))
                .build()
                .unwrap(),
        }
    }

    pub async fn generate_image(&self, body: &Value) -> anyhow::Result<Value> {
        let url = format!("{}/v1/images/generations", self.api_base);
        let resp = self.client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("image generation failed with status {status}: {text}");
        }
        let result: Value = resp.json().await?;
        Ok(result)
    }
}
```

- [ ] **Step 3: Create `src/api/routes/images.rs`**

```rust
use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;
use crate::api::{app::AppState, auth::AuthenticatedUser, error::ApiError};
use crate::router::policy::PolicyDecision;

pub async fn image_generations(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};
    use crate::db::models::{NewCostLedgerEntry, NewPrompt};

    let user = user.0;
    let model = body["model"].as_str().unwrap_or("dall-e-3").to_string();
    let quality = body["quality"].as_str().unwrap_or("standard").to_string();
    let n_images = body["n"].as_u64().unwrap_or(1) as u32;

    match state.policy.check(&user, &model).await? {
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
        PolicyDecision::Allow { .. } => {}
    }

    let provider_name = state.settings.routing.default_provider.clone();
    let provider_config = state.settings.providers.get(&provider_name)
        .cloned()
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.clone()))?;

    let adapter = crate::providers::openai_images::OpenAIImageAdapter::new(&provider_config);
    let start = std::time::Instant::now();
    let result = adapter.generate_image(&body).await
        .map_err(|e| ApiError::ProviderError(e.to_string()))?;
    let latency_ms = start.elapsed().as_millis() as i64;

    // Per-image cost lookup: pricing key is "dall-e-3/standard" or "dall-e-2/standard" etc.
    let price_key = format!("{}/{}", model, quality);
    let cost_per_image = state.settings.pricing.iter()
        .find(|p| p.model == price_key)
        .map(|p| p.input_per_million) // reuse field as "cost per image" for image models
        .unwrap_or(if model.contains("dall-e-3") && quality == "hd" { 0.080 }
                   else if model.contains("dall-e-3") { 0.040 }
                   else { 0.020 });
    let cost = cost_per_image * n_images as f64;

    let state_c = state.clone();
    let model_c = model.clone();
    let provider_c = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id, session_id: None,
            request_model: model_c.clone(), routed_model: model_c.clone(),
            provider: provider_c.clone(),
            messages: serde_json::to_string(&serde_json::json!({"prompt": "image generation"})).unwrap_or_default(),
            response: None, finish_reason: None,
            prompt_tokens: n_images as i64, completion_tokens: 0,
            cost_usd: cost, latency_ms: Some(latency_ms),
            tags: "[]".to_string(), project: None,
        };
        match PromptRepository::create(&*state_c.db, prompt).await {
            Ok(saved_prompt) => {
                let ledger = NewCostLedgerEntry {
                    user_id, prompt_id: saved_prompt.id,
                    model: model_c, provider: provider_c, project: None,
                    tokens_in: n_images as i64, tokens_out: 0,
                    cost_usd: cost, api_key_id,
                };
                if let Err(e) = CostRepository::create(&*state_c.db, ledger).await {
                    tracing::error!("failed to record image cost: {e}");
                }
            }
            Err(e) => tracing::error!("failed to record image prompt: {e}"),
        }
    });

    Ok(axum::Json(result).into_response())
}
```

- [ ] **Step 4: Register route in `src/api/app.rs`**

```rust
use crate::api::routes::images::image_generations;
// in build_router:
.route("/v1/images/generations", post(image_generations))
```

- [ ] **Step 5: Run tests**

Run: `cargo test test_images`
Expected: PASS (401 test passes; no authenticated test since that would require a real API key)

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add src/providers/openai_images.rs src/api/routes/images.rs src/api/routes/mod.rs src/api/app.rs tests/test_images.rs
git commit -m "feat: /v1/images/generations endpoint with DALL-E backend and per-image cost logging"
```

---

### Task 7: Audio Transcription and Speech Endpoints (Gap #33)

Add `POST /v1/audio/speech` (TTS) and `POST /v1/audio/transcriptions` (Whisper) as passthroughs to OpenAI.

**Files:**
- Create: `src/api/routes/audio.rs`
- Modify: `src/api/routes/mod.rs`
- Modify: `src/api/app.rs` — register routes
- Test: `tests/test_audio.rs`

**Pitfalls:**
- `POST /v1/audio/transcriptions` uses `multipart/form-data`. Use `axum::extract::Multipart`.
- `POST /v1/audio/speech` returns binary audio (MP3 bytes), NOT JSON. Return with content-type `audio/mpeg`.
- Cost logging: use `NewPrompt` + `NewCostLedgerEntry` with the same fire-and-forget pattern. For TTS, `tokens_in = char_count`, `tokens_out = 0`. For transcriptions, `tokens_in = 1` (unknown duration).
- `policy.check(&user, &model)` required in both handlers.
- Test only covers unauthenticated case (401) — authenticated test requires live API key.

- [ ] **Step 1: Write failing test**

Create `tests/test_audio.rs`:
```rust
mod common;

#[tokio::test]
async fn audio_speech_unauthenticated_returns_401() {
    let server = common::test_app().await;
    let resp = server
        .post("/v1/audio/speech")
        .json(&serde_json::json!({"model": "tts-1", "input": "Hello world", "voice": "alloy"}))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn audio_transcription_unauthenticated_returns_401() {
    let server = common::test_app().await;
    let resp = server.post("/v1/audio/transcriptions").await;
    assert_eq!(resp.status_code(), 401);
}
```

Run: `cargo test test_audio`
Expected: fail (404, routes not registered)

- [ ] **Step 2: Create `src/api/routes/audio.rs`**

```rust
use axum::{
    extract::{Multipart, State},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use crate::api::{app::AppState, auth::AuthenticatedUser, error::ApiError};
use crate::router::policy::PolicyDecision;
use crate::db::models::{NewCostLedgerEntry, NewPrompt};

pub async fn speech(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let model = body["model"].as_str().unwrap_or("tts-1").to_string();

    match state.policy.check(&user, &model).await? {
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
        PolicyDecision::Allow { .. } => {}
    }

    let provider_name = state.settings.routing.default_provider.clone();
    let provider_config = state.settings.providers.get(&provider_name)
        .cloned()
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.clone()))?;

    let api_base = provider_config.api_base.clone()
        .unwrap_or_else(|| "https://api.openai.com".to_string());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(provider_config.timeout_secs))
        .build()
        .unwrap();

    let start = std::time::Instant::now();
    let resp = client
        .post(format!("{}/v1/audio/speech", api_base))
        .bearer_auth(&provider_config.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| ApiError::ProviderError(e.to_string()))?;
    let latency_ms = start.elapsed().as_millis() as i64;

    if !resp.status().is_success() {
        return Err(ApiError::ProviderError(format!("TTS failed: {}", resp.status())));
    }

    let char_count = body["input"].as_str().map(|s| s.len()).unwrap_or(0);
    let cost = (char_count as f64 / 1000.0) * 0.015; // $0.015 per 1K chars

    let state_c = state.clone();
    let model_c = model.clone();
    let provider_c = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id, session_id: None,
            request_model: model_c.clone(), routed_model: model_c.clone(),
            provider: provider_c.clone(),
            messages: format!("{{\"chars\":{}}}", char_count),
            response: None, finish_reason: None,
            prompt_tokens: char_count as i64, completion_tokens: 0,
            cost_usd: cost, latency_ms: Some(latency_ms),
            tags: "[]".to_string(), project: None,
        };
        match PromptRepository::create(&*state_c.db, prompt).await {
            Ok(saved_prompt) => {
                let _ = CostRepository::create(&*state_c.db, NewCostLedgerEntry {
                    user_id, prompt_id: saved_prompt.id,
                    model: model_c, provider: provider_c, project: None,
                    tokens_in: char_count as i64, tokens_out: 0,
                    cost_usd: cost, api_key_id,
                }).await;
            }
            Err(e) => tracing::error!("failed to record TTS cost: {e}"),
        }
    });

    let content_type = resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/mpeg")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| ApiError::ProviderError(e.to_string()))?;
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response())
}

pub async fn transcriptions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};

    let user = user.0;
    let model = "whisper-1".to_string();

    match state.policy.check(&user, &model).await? {
        PolicyDecision::Deny { reason, status, .. } => {
            return Err(ApiError::PolicyDenied { reason, status });
        }
        PolicyDecision::Allow { .. } => {}
    }

    let provider_name = state.settings.routing.default_provider.clone();
    let provider_config = state.settings.providers.get(&provider_name)
        .cloned()
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.clone()))?;

    let api_base = provider_config.api_base.clone()
        .unwrap_or_else(|| "https://api.openai.com".to_string());

    let mut form = reqwest::multipart::Form::new();
    while let Some(field) = multipart.next_field().await
        .map_err(|_| ApiError::BadRequest("invalid multipart".to_string()))?
    {
        let name = field.name().unwrap_or("file").to_string();
        let filename = field.file_name().map(|s| s.to_string());
        let content_type = field.content_type().map(|s| s.to_string());
        let bytes = field.bytes().await
            .map_err(|_| ApiError::BadRequest("multipart read error".to_string()))?;

        let mut part = reqwest::multipart::Part::bytes(bytes.to_vec());
        if let Some(fname) = filename {
            part = part.file_name(fname);
        }
        if let Some(ct) = content_type {
            part = part.mime_str(&ct).unwrap_or(part);
        }
        form = form.part(name, part);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(provider_config.timeout_secs))
        .build()
        .unwrap();

    let start = std::time::Instant::now();
    let resp = client
        .post(format!("{}/v1/audio/transcriptions", api_base))
        .bearer_auth(&provider_config.api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| ApiError::ProviderError(e.to_string()))?;
    let latency_ms = start.elapsed().as_millis() as i64;

    let result: Value = resp.json().await
        .map_err(|e| ApiError::ProviderError(e.to_string()))?;

    let cost = 0.006; // $0.006/min baseline — duration unknown without parsing
    let state_c = state.clone();
    let model_c = model.clone();
    let provider_c = provider_name.clone();
    let user_id = user.id;
    let api_key_id = user.api_key_id;
    tokio::spawn(async move {
        let prompt = NewPrompt {
            user_id, session_id: None,
            request_model: model_c.clone(), routed_model: model_c.clone(),
            provider: provider_c.clone(), messages: "{\"type\":\"audio\"}".to_string(),
            response: result["text"].as_str().map(|s| s.to_string()), finish_reason: None,
            prompt_tokens: 1, completion_tokens: 0,
            cost_usd: cost, latency_ms: Some(latency_ms),
            tags: "[]".to_string(), project: None,
        };
        match PromptRepository::create(&*state_c.db, prompt).await {
            Ok(saved_prompt) => {
                let _ = CostRepository::create(&*state_c.db, NewCostLedgerEntry {
                    user_id, prompt_id: saved_prompt.id,
                    model: model_c, provider: provider_c, project: None,
                    tokens_in: 1, tokens_out: 0,
                    cost_usd: cost, api_key_id,
                }).await;
            }
            Err(e) => tracing::error!("failed to record transcription cost: {e}"),
        }
    });

    Ok(axum::Json(result).into_response())
}
```

- [ ] **Step 3: Register routes in `src/api/app.rs`**

```rust
use crate::api::routes::audio::{speech, transcriptions};
// in build_router:
.route("/v1/audio/speech", post(speech))
.route("/v1/audio/transcriptions", post(transcriptions))
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add src/api/routes/audio.rs src/api/routes/mod.rs src/api/app.rs tests/test_audio.rs
git commit -m "feat: /v1/audio/speech and /v1/audio/transcriptions endpoints with cost logging"
```

---

### Task 8: Transparent Retry / Request Queuing (Gap #31)

When a provider returns a 429 or 5xx error, retry transparently with exponential backoff. Non-streaming requests only (streaming retries are a future enhancement).

**Files:**
- Create: `src/router/retry.rs`
- Modify: `src/config/schema.rs` — add `RetryConfig`
- Modify: `src/api/routes/completions.rs` — wrap non-streaming dispatch with retry
- Modify: `src/router/mod.rs` — declare `retry` module
- Modify: `Cargo.toml` — add `rand = "0.8"`
- Test: `tests/test_retry.rs`

**Pitfalls:**
- Provider errors are currently returned as `anyhow::Error`. The retry logic can only detect rate limits if providers embed the status code in the error message string. Document this as a known limitation — a follow-up phase should add typed provider errors.
- Only retry non-streaming requests: check `if !stream { ... retry loop ... } else { ... single attempt ... }`.
- `max_retries = 0` disables retry (backward compat).
- `rand` crate must be added to `[dependencies]`, not `[dev-dependencies]`.

- [ ] **Step 1: Add `RetryConfig` to `src/config/schema.rs`**

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_delay_ms")]
    pub base_delay_ms: u64,
    #[serde(default = "default_retry_max_delay_ms")]
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            base_delay_ms: default_retry_base_delay_ms(),
            max_delay_ms: default_retry_max_delay_ms(),
        }
    }
}

fn default_max_retries() -> u32 { 3 }
fn default_retry_base_delay_ms() -> u64 { 1000 }
fn default_retry_max_delay_ms() -> u64 { 30000 }
```

Add to `Settings`:
```rust
#[serde(default)]
pub retry: RetryConfig,
```

- [ ] **Step 2: Add `rand = "0.8"` to `Cargo.toml` `[dependencies]`**

- [ ] **Step 3: Write failing tests**

Create `tests/test_retry.rs`:
```rust
use modelrouter::router::retry::{RetryPolicy, RetryableError};

#[test]
fn allows_up_to_max_retries() {
    let policy = RetryPolicy::new(3, 100, 1000);
    assert!(policy.should_retry(0, &RetryableError::RateLimit));
    assert!(policy.should_retry(2, &RetryableError::RateLimit));
    assert!(!policy.should_retry(3, &RetryableError::RateLimit));
}

#[test]
fn does_not_retry_auth_errors() {
    let policy = RetryPolicy::new(3, 100, 1000);
    assert!(!policy.should_retry(0, &RetryableError::AuthError));
    assert!(!policy.should_retry(0, &RetryableError::NotRetryable));
}

#[test]
fn delay_increases_with_attempt_number() {
    let policy = RetryPolicy::new(3, 100, 100_000);
    // Each delay should be at least 1.5x the previous (2x minus jitter)
    let d0 = policy.delay_ms(0);
    let d1 = policy.delay_ms(1);
    let d2 = policy.delay_ms(2);
    assert!(d0 >= 50, "d0={d0}");   // base 100ms ± 10%
    assert!(d1 >= d0, "d1={d1} d0={d0}");
    assert!(d2 >= d1, "d2={d2} d1={d1}");
}

#[test]
fn delay_is_capped_at_max() {
    let policy = RetryPolicy::new(10, 1000, 5000);
    // After many doublings, delay should not exceed max
    for attempt in 5..10 {
        assert!(policy.delay_ms(attempt) <= 5500, "exceeded max at attempt {attempt}");
    }
}
```

Run: `cargo test test_retry`
Expected: compile error

- [ ] **Step 4: Create `src/router/retry.rs`**

```rust
use crate::config::schema::RetryConfig;

#[derive(Debug)]
pub enum RetryableError {
    RateLimit,
    ServerError(u16),
    AuthError,
    NotRetryable,
}

impl RetryableError {
    pub fn classify(err_str: &str) -> Self {
        // Known limitation: providers currently embed status codes in error strings.
        // A future phase should add typed ProviderError variants for reliable classification.
        if err_str.contains("429") || err_str.to_lowercase().contains("rate limit") {
            Self::RateLimit
        } else if err_str.contains("500") || err_str.contains("502") || err_str.contains("503") {
            Self::ServerError(500)
        } else if err_str.contains("401") || err_str.contains("403") {
            Self::AuthError
        } else {
            Self::NotRetryable
        }
    }
}

pub struct RetryPolicy {
    max_retries: u32,
    base_delay_ms: u64,
    max_delay_ms: u64,
}

impl RetryPolicy {
    pub fn new(max_retries: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        Self { max_retries, base_delay_ms, max_delay_ms }
    }

    pub fn from_config(config: &RetryConfig) -> Self {
        Self::new(config.max_retries, config.base_delay_ms, config.max_delay_ms)
    }

    pub fn should_retry(&self, attempt: u32, error: &RetryableError) -> bool {
        if attempt >= self.max_retries {
            return false;
        }
        matches!(error, RetryableError::RateLimit | RetryableError::ServerError(_))
    }

    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let base = self.base_delay_ms.saturating_mul(2u64.saturating_pow(attempt));
        let capped = base.min(self.max_delay_ms);
        let jitter_range = (capped / 10).max(1);
        let jitter = rand::random::<u64>() % (jitter_range * 2);
        capped.saturating_sub(jitter_range).saturating_add(jitter)
    }
}
```

- [ ] **Step 5: Run retry tests**

Run: `cargo test test_retry`
Expected: PASS

- [ ] **Step 6: Apply retry in `src/api/routes/completions.rs` for non-streaming requests**

Find the section that dispatches to the provider for the non-streaming path and wrap it:
```rust
// Replace single provider.complete() call with retry loop (non-streaming only):
let retry_policy = crate::router::retry::RetryPolicy::from_config(&state.settings.retry);
let mut attempt = 0u32;
let result = loop {
    match provider.complete(&canonical_model, &body).await {
        Ok(r) => break Ok(r),
        Err(e) => {
            let err_str = e.to_string();
            let retryable = crate::router::retry::RetryableError::classify(&err_str);
            if retry_policy.should_retry(attempt, &retryable) {
                let delay = retry_policy.delay_ms(attempt);
                tracing::warn!("provider error on attempt {attempt}, retrying in {delay}ms: {err_str}");
                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                attempt += 1;
                continue;
            }
            break Err(e);
        }
    }
};
let result = result.map_err(|e| ApiError::ProviderError(e.to_string()))?;
```

- [ ] **Step 7: Run all tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 8: Commit**

```bash
git add src/router/retry.rs src/router/mod.rs src/config/schema.rs src/api/routes/completions.rs Cargo.toml tests/test_retry.rs
git commit -m "feat: transparent retry with exponential backoff on 429/5xx provider errors"
```

---

## Group B: Sub-Project Stubs

These items each require a dedicated planning session before implementation. The stubs below document scope, proposed interfaces, and integration points.

---

### Stub B1: Guardrail Framework (Gap #23)

**Scope:** Pre-call and post-call hook interface with a guardrails trait. Guardrail checks run before request dispatch and optionally after response; a failed check can block or replace.

**Proposed trait:**
```rust
// src/guardrails/mod.rs
#[async_trait::async_trait]
pub trait Guardrail: Send + Sync {
    fn name(&self) -> &str;
    async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision;
    async fn check_response(&self, ctx: &GuardrailContext, response: &str) -> GuardrailDecision;
}

pub enum GuardrailDecision {
    Allow,
    Block { reason: String },
    Replace { content: String },
}
```

**Config:**
```toml
[[guardrails]]
name = "openai-moderation"
type = "openai_moderation"
fail_open = true

[[guardrails]]
name = "pii-masker"
type = "presidio"
endpoint = "http://presidio:3000"
fail_open = false
```

**AppState addition:** `guardrails: Arc<GuardrailChain>`. Called in `completions.rs` before and after provider dispatch.

**Estimated effort:** 2-3 days for framework + 1 built-in guardrail (OpenAI moderation).

---

### Stub B2: Kubernetes / Helm Charts (Gap #26)

**Scope:** Helm chart at `deploy/helm/modelrouter/` with Deployment, HPA, PVC for SQLite, ConfigMap for config.toml, Secret for API keys, init container for migrations, and `values.yaml`.

**Prerequisites:** Dockerfile published to a registry. Update `/health` to return appropriate probe-compatible status codes.

**Estimated effort:** 1 day. Pure ops work.

---

### Stub B3: MCP Server Registry (Gap #27)

**Scope:** Full CRUD for MCP server registrations + semantic tool filtering.

**New endpoints:** `POST/GET/PUT/DELETE /v1/mcp/server`, `GET /v1/mcp/discover`

**New migration:**
```sql
CREATE TABLE mcp_servers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    url TEXT NOT NULL,
    description TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

**Semantic filtering:** Before injecting tools into a request, embed the user prompt and each tool description; cosine-similarity select top-K tools. Uses existing `EmbeddingRegistry`.

**Estimated effort:** 3-4 days.

---

### Stub B4: Declarative Policy Engine (Gap #28)

**Scope:** Replace hard-coded budget/rate checks in `router/policy.rs` with a condition-based rule evaluator.

**Proposed rule format (TOML):**
```toml
[[policy.rules]]
name = "research-team-opus"
condition = { tag = "research" }
allow_models = ["claude-opus-4-5"]
budget_usd = 200.0
window = "monthly"
priority = 10
```

**Rule evaluation:** Sort by priority descending, first matching rule wins. Conditions match on `tag`, `group_name`, `user_id`, `model`.

**Estimated effort:** 4-5 days. Must maintain backward compatibility with existing `budget_rules` table.

---

### Stub B5: SSO / OIDC (Gap #34)

**Scope:** OIDC callback flow for the admin dashboard. Supports Okta, Azure AD, Auth0.

**Flow:** Login → redirect to IdP → callback `GET /admin/auth/callback?code=...` → exchange code → validate ID token → issue `DashboardSession` cookie.

**Config:**
```toml
[auth.oidc]
provider = "okta"
client_id = "..."
client_secret = "..."
redirect_uri = "https://router.internal/admin/auth/callback"
issuer = "https://my-company.okta.com"
```

**Dependency:** `openidconnect` crate.

**Estimated effort:** 3-4 days. Cookie session management already exists.

---

### Stub B6: SCIM Provisioning (Gap #35)

**Scope:** SCIM 2.0 endpoints for user and group sync from an IdP.

**New endpoints:** `GET/POST /scim/v2/Users`, `GET/PUT/PATCH/DELETE /scim/v2/Users/:id`, `GET/POST/PUT /scim/v2/Groups`, `PUT /scim/v2/Groups/:id`

**Auth:** Long-lived SCIM bearer token configured separately from user API keys.

**Estimated effort:** 3-4 days. Main complexity is the PATCH operation (JSON Patch for field-level updates).

---

### Stub B7: Shadow Traffic Routing (Gap #36)

**Scope:** Mirror a configurable fraction of live requests to a shadow provider without affecting the user-facing response.

**Config:**
```toml
[routing.shadow]
enabled = true
fraction = 0.05
provider = "anthropic"
model = "claude-opus-4-5"
```

**Implementation:** After returning the primary response, `tokio::spawn` a background request to the shadow provider. Log shadow results to a `shadow_log` table for comparison.

**Estimated effort:** 2 days.

---

### Stub B8: Billing Integrations (Gap #37)

**Scope:** Push usage events to Stripe (metered billing) and/or Lago (open-source billing) after each request.

**Pattern:** Same fire-and-forget pattern as LangFuse/LangSmith callbacks (Task 3). New backends added to `CallbackDispatcher`. Requires storing `stripe_customer_id` / `lago_subscription_id` per user (new migration).

**Estimated effort:** 2-3 days, assuming Task 3 is complete.

---

### Stub B9: Agent Endpoints (Gap #38)

**Scope:** Store agent configurations (system prompt, tools, model, budget) and execute them with session memory and per-session rate limiting.

**New endpoints:** `POST/GET /v1/agents`, `POST /v1/agents/:id/run`, `GET /v1/agents/:id/runs`

**New tables:** `agents` (config), `agent_sessions` (conversation history per session_id)

**Estimated effort:** 5-7 days. Depends on Task 4 (session limits) being complete.

---

### Stub B10: Vector Stores and RAG (Gap #39)

**Scope:** Manage vector stores for RAG pipelines through the proxy.

**New endpoints:** `POST/GET /v1/vector_stores`, `POST /v1/vector_stores/:id/files`, `POST /v1/vector_stores/:id/search`, `DELETE /v1/vector_stores/:id`

**Storage:** SQLite for metadata; embeddings stored as BLOBs with cosine similarity in Rust for MVP. Optional: pgvector for production.

**Estimated effort:** 7-10 days. Significant new subsystem.

---

### Stub B11: Realtime WebSocket API (Gap #40)

**Scope:** WebSocket proxy for OpenAI Realtime API.

**Endpoint:** `GET /v1/realtime` (WebSocket upgrade)

**Implementation:** Upgrade HTTP to WebSocket via `axum::extract::ws::WebSocketUpgrade`, establish WebSocket to upstream provider, bidirectionally proxy messages. Auth validated before upgrade. Cost estimated at session end from audio duration in final message metadata.

**Estimated effort:** 5-7 days.

---

### Batch API Note (Gap #30)

The Batch API (`POST /v1/batches`) requires:
- New `batches` and `batch_items` tables
- Background job polling OpenAI for batch completion and logging costs
- `GET /v1/batches/:id` for status polling

Fully implementable from existing patterns. Should be planned as a standalone phase when batch workloads are a priority.

---

## Execution Order

Recommended order for Group A tasks (each independently shippable):

1. **Task 5** (Responses API) — lowest risk, 1 new file
2. **Task 2** (Hot-reload) — unblocks zero-downtime config changes
3. **Task 4** (Session limits) — agent loop safety
4. **Task 8** (Retry/queuing) — reliability improvement
5. **Task 3** (LangFuse/LangSmith) — observability
6. **Task 6** (Image generation) — new endpoint, minimal risk
7. **Task 7** (Audio) — new endpoints, minimal risk
8. **Task 1** (Cold storage) — operational hygiene

Group B priority order:
B2 (Helm) → B5 (SSO) → B1 (Guardrails) → B4 (Policy engine) → B3 (MCP) → B7 (Shadow) → B8 (Billing, needs Task 3) → B6 (SCIM) → B9 (Agents, needs Task 4) → B10 (Vector stores) → B11 (Realtime)
