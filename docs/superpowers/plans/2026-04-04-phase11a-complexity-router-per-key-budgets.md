# Phase 11a: Complexity Router + Per-Key Budgets

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add automatic cheap-model routing based on estimated token count (Task 11.1), and support multiple API keys per user with per-key budget rules (Task 11.5).

**Architecture:** ComplexityRouter sits in the request path before provider dispatch — it estimates tokens from message content and may substitute a cheaper model. Per-key budgets add a new `api_keys` table; auth tries key lookup first, then falls back to legacy `users.api_key`; PolicyEngine checks key-specific rules when a key is matched.

**Tech Stack:** Rust 2021, axum 0.7, sqlx 0.8, serde_json, tokio, sha2, hex

---

## File Map

### Task 11.1 — Complexity Router

| File | Action | Responsibility |
|------|--------|----------------|
| `src/config/schema.rs` | Modify | Add `ComplexityRoutingConfig` struct and optional field to `RoutingConfig` |
| `src/router/complexity.rs` | Create | `ComplexityRouter` struct with `maybe_downgrade()` |
| `src/router/mod.rs` | Modify | Declare `pub mod complexity;` |
| `src/api/app.rs` | Modify | Add `complexity_router: Arc<ComplexityRouter>` to `AppState` and `DatabaseProvider` |
| `src/api/routes/completions.rs` | Modify | Call `complexity_router.maybe_downgrade()` before resolving provider |
| `src/api/routes/messages.rs` | Modify | Same — call `maybe_downgrade()` for Anthropic path |
| `src/cli/mod.rs` | Modify | Construct `ComplexityRouter` from settings and inject into `AppState` |
| `tests/test_complexity.rs` | Create | Unit + integration tests for complexity routing |

### Task 11.5 — Per-Key Budgets

| File | Action | Responsibility |
|------|--------|----------------|
| `migrations/002_per_key_budgets.sql` | Create | `api_keys` table; add `api_key_id` column to `budget_rules` |
| `src/db/models.rs` | Modify | Add `ApiKey`, `NewApiKey`; add `api_key_id: Option<i64>` to `User` |
| `src/db/repositories/api_keys.rs` | Create | `ApiKeyRepository` trait |
| `src/db/repositories/mod.rs` | Modify | Declare `pub mod api_keys;` |
| `src/db/repositories/budgets.rs` | Modify | Add `list_for_key(api_key_id: i64)` to `BudgetRepository` |
| `src/db/sqlite/api_keys.rs` | Create | SQLite impl of `ApiKeyRepository` |
| `src/db/sqlite/mod.rs` | Modify | Declare `pub mod api_keys;` |
| `src/db/sqlite/budgets.rs` | Modify | Implement `list_for_key` |
| `src/api/app.rs` | Modify | Add `ApiKeyRepository` to `DatabaseProvider` supertrait bounds |
| `src/api/auth.rs` | Modify | Try `api_keys` lookup first; set `api_key_id` on returned `User` |
| `src/router/policy.rs` | Modify | Check key-specific budget rules when `user.api_key_id.is_some()` |
| `src/api/admin/routes.rs` | Modify | Add REST endpoints for API key management |
| `src/api/app.rs` | Modify | Register new admin routes |
| `tests/test_per_key_budgets.rs` | Create | Integration tests for key auth and per-key budget enforcement |

---

## Task 1: Complexity Router

**Files:**
- Create: `src/router/complexity.rs`
- Modify: `src/config/schema.rs`
- Modify: `src/router/mod.rs`
- Modify: `src/api/app.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `src/api/routes/messages.rs`
- Modify: `src/cli/mod.rs`
- Create: `tests/test_complexity.rs`

---

- [ ] **Step 1: Write failing unit tests for token estimation and downgrade logic**

Create `tests/test_complexity.rs`:

```rust
mod common;

use modelrouter::router::complexity::ComplexityRouter;
use modelrouter::config::schema::{ComplexityRoutingConfig, RoutingConfig};
use serde_json::json;

fn config_with_threshold(threshold: u32, cheap_model: &str) -> ComplexityRoutingConfig {
    ComplexityRoutingConfig {
        enabled: true,
        token_threshold: threshold,
        cheap_model: cheap_model.to_string(),
    }
}

#[test]
fn short_messages_stay_on_requested_model() {
    let config = config_with_threshold(100, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    let messages = vec![json!({"role": "user", "content": "Hi"})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}

#[test]
fn long_messages_downgrade_to_cheap_model() {
    let config = config_with_threshold(10, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    // "A".repeat(200) → 200 chars / 4 = 50 tokens, > threshold of 10
    let content = "A".repeat(200);
    let messages = vec![json!({"role": "user", "content": content})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o-mini");
}

#[test]
fn disabled_config_never_downgrades() {
    let config = ComplexityRoutingConfig {
        enabled: false,
        token_threshold: 1,
        cheap_model: "gpt-4o-mini".to_string(),
    };
    let router = ComplexityRouter::new(Some(config));
    let content = "A".repeat(1000);
    let messages = vec![json!({"role": "user", "content": content})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}

#[test]
fn none_config_never_downgrades() {
    let router = ComplexityRouter::new(None);
    let content = "A".repeat(1000);
    let messages = vec![json!({"role": "user", "content": content})];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o");
}

#[test]
fn multi_message_tokens_summed() {
    let config = config_with_threshold(10, "gpt-4o-mini");
    let router = ComplexityRouter::new(Some(config));
    // Two messages each with 40 chars = 80 chars / 4 = 20 tokens total, > 10
    let messages = vec![
        json!({"role": "user", "content": "A".repeat(40)}),
        json!({"role": "assistant", "content": "B".repeat(40)}),
    ];
    assert_eq!(router.maybe_downgrade("gpt-4o", &messages), "gpt-4o-mini");
}

#[test]
fn estimate_tokens_counts_chars_over_four() {
    // 400 chars → 100 tokens
    assert_eq!(
        modelrouter::router::complexity::estimate_tokens_from_messages(
            &[json!({"role": "user", "content": "A".repeat(400)})]
        ),
        100
    );
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --test test_complexity 2>&1 | head -30
```

Expected: compile error — module `complexity` does not exist.

- [ ] **Step 3: Add `ComplexityRoutingConfig` to config schema**

In `src/config/schema.rs`, add after the `PricingEntry` struct:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ComplexityRoutingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_complexity_threshold")]
    pub token_threshold: u32,
    #[serde(default = "default_cheap_model")]
    pub cheap_model: String,
}

fn default_complexity_threshold() -> u32 { 500 }
fn default_cheap_model() -> String { "gpt-4o-mini".to_string() }
```

Then add the field to `RoutingConfig`:

```rust
pub struct RoutingConfig {
    // ... existing fields ...
    #[serde(default)]
    pub complexity_routing: Option<ComplexityRoutingConfig>,
}
```

And update `RoutingConfig::default()`:

```rust
impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            default_model: default_model(),
            model_aliases: HashMap::new(),
            fallback_chains: HashMap::new(),
            complexity_routing: None,
        }
    }
}
```

- [ ] **Step 4: Create `src/router/complexity.rs`**

```rust
use serde_json::Value;
use crate::config::schema::ComplexityRoutingConfig;

pub struct ComplexityRouter {
    config: Option<ComplexityRoutingConfig>,
}

impl ComplexityRouter {
    pub fn new(config: Option<ComplexityRoutingConfig>) -> Self {
        Self { config }
    }

    /// Returns the model to use — either the requested model or the cheap model
    /// if token count exceeds the configured threshold.
    pub fn maybe_downgrade(&self, requested_model: &str, messages: &[Value]) -> String {
        let config = match &self.config {
            Some(c) if c.enabled => c,
            _ => return requested_model.to_string(),
        };

        let estimated = estimate_tokens_from_messages(messages);
        if estimated > config.token_threshold as usize {
            config.cheap_model.clone()
        } else {
            requested_model.to_string()
        }
    }
}

/// Estimate token count from messages using chars/4 heuristic.
pub fn estimate_tokens_from_messages(messages: &[Value]) -> usize {
    messages.iter().map(|m| {
        m["content"].as_str().map(|s| s.chars().count() / 4).unwrap_or(0)
    }).sum()
}
```

- [ ] **Step 5: Declare module in `src/router/mod.rs`**

Add `pub mod complexity;` to the existing module declarations.

- [ ] **Step 6: Run tests to confirm they pass**

```bash
cargo test --test test_complexity
```

Expected: all 6 tests pass.

- [ ] **Step 7: Add `complexity_router` to `AppState`**

In `src/api/app.rs`, add to the `AppState` struct:

```rust
pub complexity_router: Arc<crate::router::complexity::ComplexityRouter>,
```

- [ ] **Step 8: Wire `complexity_router` into `chat_completions`**

In `src/api/routes/completions.rs`, inside `chat_completions_inner`, replace the line:

```rust
let model = body["model"]
    .as_str()
    .unwrap_or(&state.settings.routing.default_model)
    .to_string();
```

with:

```rust
let requested_model = body["model"]
    .as_str()
    .unwrap_or(&state.settings.routing.default_model)
    .to_string();
let messages_for_complexity = body["messages"].as_array().cloned().unwrap_or_default();
let model = state.complexity_router.maybe_downgrade(&requested_model, &messages_for_complexity);
```

- [ ] **Step 9: Wire `complexity_router` into `anthropic_messages`**

In `src/api/routes/messages.rs`, after extracting `model` from the body, add:

```rust
let messages_for_complexity = body["messages"].as_array().cloned().unwrap_or_default();
let model = state.complexity_router.maybe_downgrade(&model, &messages_for_complexity);
```

(replace `let model = ...` with the two-step version that first assigns to `requested_model`, then calls `maybe_downgrade`)

- [ ] **Step 10: Construct `ComplexityRouter` in `src/cli/mod.rs`**

Find where `AppState` is built and add:

```rust
let complexity_router = Arc::new(ComplexityRouter::new(
    settings.routing.complexity_routing.clone()
));
```

Add `use modelrouter::router::complexity::ComplexityRouter;` to imports, and add `complexity_router` to the `AppState { ... }` initializer.

Also update `tests/test_completions.rs` `test_app()` to include:

```rust
let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
```

and add `complexity_router` to the `AppState { ... }` struct literal.

- [ ] **Step 11: Build and run all tests**

```bash
cargo build && cargo test
```

Expected: all tests pass, no compile errors.

- [ ] **Step 12: Commit**

```bash
git add src/config/schema.rs src/router/complexity.rs src/router/mod.rs \
        src/api/app.rs src/api/routes/completions.rs src/api/routes/messages.rs \
        src/cli/mod.rs tests/test_complexity.rs tests/test_completions.rs
git commit -m "feat: add complexity router — auto-downgrade to cheap model when token estimate exceeds threshold"
```

---

## Task 2: Per-Key Budgets

**Files:**
- Create: `migrations/002_per_key_budgets.sql`
- Modify: `src/db/models.rs`
- Create: `src/db/repositories/api_keys.rs`
- Modify: `src/db/repositories/mod.rs`
- Modify: `src/db/repositories/budgets.rs`
- Create: `src/db/sqlite/api_keys.rs`
- Modify: `src/db/sqlite/mod.rs`
- Modify: `src/db/sqlite/budgets.rs`
- Modify: `src/api/app.rs`
- Modify: `src/api/auth.rs`
- Modify: `src/router/policy.rs`
- Modify: `src/api/app.rs` (routes)
- Create: `tests/test_per_key_budgets.rs`

---

- [ ] **Step 1: Write failing integration tests**

Create `tests/test_per_key_budgets.rs`:

```rust
mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::Settings;
use modelrouter::db::models::{NewUser, NewApiKey};
use modelrouter::db::repositories::users::UserRepository;
use modelrouter::db::repositories::api_keys::ApiKeyRepository;
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{
    cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain,
    policy::PolicyEngine, complexity::ComplexityRouter,
};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app() -> (TestServer, Arc<dyn DatabaseProvider>) {
    let db = common::in_memory_db().await;

    db.create(NewUser {
        name: "base-user".to_string(),
        api_key_hash: hash_token("legacy-token"),
        group_name: None,
    })
    .await
    .unwrap();

    let settings = Arc::new(Settings::default());
    let db: Arc<dyn DatabaseProvider> = Arc::new(db);
    let router = Arc::new(RequestRouter::new(settings.clone()));
    let cost_calc = Arc::new(CostCalculator::new());
    let provider_registry = Arc::new(ProviderRegistry::new_with_mock(common::MockAdapter {
        response: "Hello!".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(ComplexityRouter::new(None));

    let state = AppState {
        settings,
        db: db.clone(),
        pool: None,
        router,
        cost_calc,
        provider_registry,
        policy,
        fallback,
        complexity_router,
        app_metrics: None,
    };
    (TestServer::new(build_router(state)).unwrap(), db)
}

#[tokio::test]
async fn api_key_auth_works() {
    let (server, db) = test_app().await;

    // Create an API key for the base user
    let user = db.find_by_name("base-user").await.unwrap().unwrap();
    db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: hash_token("per-key-token"),
        label: Some("test-key".to_string()),
    })
    .await
    .unwrap();

    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer per-key-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn legacy_token_still_works() {
    let (server, _db) = test_app().await;

    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer legacy-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
}

#[tokio::test]
async fn revoked_api_key_returns_401() {
    let (server, db) = test_app().await;
    let user = db.find_by_name("base-user").await.unwrap().unwrap();

    let key = db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: hash_token("revokable-token"),
        label: None,
    })
    .await
    .unwrap();

    db.revoke_api_key(key.id).await.unwrap();

    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer revokable-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hi"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 401);
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --test test_per_key_budgets 2>&1 | head -40
```

Expected: compile errors — `NewApiKey`, `ApiKeyRepository` not found.

- [ ] **Step 3: Create migration `migrations/002_per_key_budgets.sql`**

```sql
-- migrations/002_per_key_budgets.sql
CREATE TABLE IF NOT EXISTS api_keys (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key_hash   TEXT NOT NULL UNIQUE,
    label      TEXT,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_api_keys_user ON api_keys(user_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);

ALTER TABLE budget_rules ADD COLUMN api_key_id INTEGER REFERENCES api_keys(id) ON DELETE CASCADE;
```

- [ ] **Step 4: Add `ApiKey` and `NewApiKey` models, update `User`**

In `src/db/models.rs`:

Add these structs:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: i64,
    pub user_id: i64,
    pub key_hash: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug)]
pub struct NewApiKey {
    pub user_id: i64,
    pub key_hash: String,
    pub label: Option<String>,
}
```

Add `api_key_id` transient field to `User` (NOT a DB column; populated during auth):

```rust
pub struct User {
    pub id: i64,
    pub name: String,
    pub api_key: String,
    pub api_key_old: Option<String>,
    pub api_key_old_expires_at: Option<String>,
    pub group_name: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub metadata: String,
    /// Set during authentication when matched via api_keys table; None for legacy key auth.
    #[sqlx(default)]
    pub api_key_id: Option<i64>,
}
```

Note: The `sqlx::FromRow` derive on `User` already works because `api_key_id` uses `#[sqlx(default)]`. The `UserRow` intermediate type in `src/db/sqlite/users.rs` also needs to gain this field and the `From<UserRow>` impl needs to copy it.

Update `UserRow` in `src/db/sqlite/users.rs`:

```rust
struct UserRow {
    id: i64,
    name: String,
    api_key: String,
    api_key_old: Option<String>,
    api_key_old_expires_at: Option<String>,
    group_name: Option<String>,
    enabled: i64,
    created_at: String,
    metadata: String,
}
```

(No change needed — `api_key_id` is not fetched from `users` table directly; it's set after the fact in auth.)

Update `From<UserRow> for User`:

```rust
impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            name: r.name,
            api_key: r.api_key,
            api_key_old: r.api_key_old,
            api_key_old_expires_at: r.api_key_old_expires_at,
            group_name: r.group_name,
            enabled: r.enabled != 0,
            created_at: r.created_at,
            metadata: r.metadata,
            api_key_id: None,  // populated during auth if matched via api_keys
        }
    }
}
```

- [ ] **Step 5: Create `src/db/repositories/api_keys.rs`**

```rust
use async_trait::async_trait;
use crate::db::models::{ApiKey, NewApiKey};

#[async_trait]
pub trait ApiKeyRepository: Send + Sync {
    async fn find_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>>;
    async fn list_api_keys_for_user(&self, user_id: i64) -> anyhow::Result<Vec<ApiKey>>;
    async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey>;
    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<()>;
}
```

- [ ] **Step 6: Declare `api_keys` module in `src/db/repositories/mod.rs`**

Add `pub mod api_keys;` to the existing list of module declarations.

- [ ] **Step 7: Add `list_for_key` to `BudgetRepository`**

In `src/db/repositories/budgets.rs`, add to the trait:

```rust
async fn list_for_key(&self, api_key_id: i64) -> anyhow::Result<Vec<BudgetRule>>;
```

- [ ] **Step 8: Create `src/db/sqlite/api_keys.rs`**

```rust
use async_trait::async_trait;
use crate::db::models::{ApiKey, NewApiKey};
use crate::db::repositories::api_keys::ApiKeyRepository;
use super::{SqliteDb, now_utc};

#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: i64,
    user_id: i64,
    key_hash: String,
    label: Option<String>,
    enabled: i64,
    created_at: String,
}

impl From<ApiKeyRow> for ApiKey {
    fn from(r: ApiKeyRow) -> Self {
        ApiKey {
            id: r.id,
            user_id: r.user_id,
            key_hash: r.key_hash,
            label: r.label,
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

#[async_trait]
impl ApiKeyRepository for SqliteDb {
    async fn find_api_key_by_hash(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>> {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at FROM api_keys WHERE key_hash = ? AND enabled = 1"
        )
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ApiKey::from))
    }

    async fn list_api_keys_for_user(&self, user_id: i64) -> anyhow::Result<Vec<ApiKey>> {
        let rows = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at FROM api_keys WHERE user_id = ? ORDER BY id"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ApiKey::from).collect())
    }

    async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey> {
        let now = now_utc();
        let result = sqlx::query(
            "INSERT INTO api_keys (user_id, key_hash, label, enabled, created_at) VALUES (?, ?, ?, 1, ?)"
        )
        .bind(key.user_id)
        .bind(&key.key_hash)
        .bind(&key.label)
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let id = result.last_insert_rowid();
        let row = sqlx::query_as::<_, ApiKeyRow>(
            "SELECT id, user_id, key_hash, label, enabled, created_at FROM api_keys WHERE id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(ApiKey::from(row))
    }

    async fn revoke_api_key(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE api_keys SET enabled = 0 WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
```

- [ ] **Step 9: Declare `api_keys` in `src/db/sqlite/mod.rs`**

Add `pub mod api_keys;` to the module declarations.

- [ ] **Step 10: Implement `list_for_key` in `src/db/sqlite/budgets.rs`**

In the `impl BudgetRepository for SqliteDb` block, add:

```rust
async fn list_for_key(&self, api_key_id: i64) -> anyhow::Result<Vec<BudgetRule>> {
    let rows = sqlx::query_as::<_, BudgetRule>(
        r#"SELECT id, user_id, group_name, api_key_id, window, limit_usd, limit_tokens,
                  model_allow, model_deny, rate_rpm, created_at, updated_at
           FROM budget_rules WHERE api_key_id = ?"#,
    )
    .bind(api_key_id)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows)
}
```

Also update `BudgetRule` in `src/db/models.rs` to include the new column:

```rust
pub struct BudgetRule {
    pub id: i64,
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    #[sqlx(default)]
    pub api_key_id: Option<i64>,
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: String,
    pub model_deny: String,
    pub rate_rpm: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}
```

Also update `NewBudgetRule` in `src/db/models.rs` to accept `api_key_id` (so per-key rules can actually be created):

```rust
pub struct NewBudgetRule {
    pub user_id: Option<i64>,
    pub group_name: Option<String>,
    pub api_key_id: Option<i64>,    // NEW
    pub window: String,
    pub limit_usd: Option<f64>,
    pub limit_tokens: Option<i64>,
    pub model_allow: Vec<String>,
    pub model_deny: Vec<String>,
    pub rate_rpm: Option<i64>,
}
```

Update the `create` method in `src/db/sqlite/budgets.rs` to include `api_key_id` in the INSERT:

```rust
async fn create(&self, rule: NewBudgetRule) -> anyhow::Result<BudgetRule> {
    let now = now_utc();
    let model_allow = serde_json::to_string(&rule.model_allow)?;
    let model_deny = serde_json::to_string(&rule.model_deny)?;
    let result = sqlx::query(
        r#"INSERT INTO budget_rules
           (user_id, group_name, api_key_id, window, limit_usd, limit_tokens,
            model_allow, model_deny, rate_rpm, created_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(rule.user_id)
    .bind(&rule.group_name)
    .bind(rule.api_key_id)
    .bind(&rule.window)
    .bind(rule.limit_usd)
    .bind(rule.limit_tokens)
    .bind(&model_allow)
    .bind(&model_deny)
    .bind(rule.rate_rpm)
    .bind(&now)
    .bind(&now)
    .execute(&self.pool)
    .await?;

    let id = result.last_insert_rowid();
    sqlx::query_as::<_, BudgetRule>(
        r#"SELECT id, user_id, group_name, api_key_id, window, limit_usd, limit_tokens,
                  model_allow, model_deny, rate_rpm, created_at, updated_at
           FROM budget_rules WHERE id = ?"#,
    )
    .bind(id)
    .fetch_one(&self.pool)
    .await
    .map_err(Into::into)
}
```

Also update all other SELECT queries in `src/db/sqlite/budgets.rs` (`list_for_user`, `list_for_group`, `list_all`) to include `api_key_id` in the column list so sqlx can map `BudgetRule` correctly:

```sql
SELECT id, user_id, group_name, api_key_id, window, limit_usd, limit_tokens,
       model_allow, model_deny, rate_rpm, created_at, updated_at
FROM budget_rules WHERE ...
```

Note: `BudgetRule.api_key_id` uses `#[sqlx(default)]` so existing queries without the column would also compile, but being explicit is safer and avoids confusion.

Finally, any call site that constructs `NewBudgetRule` (admin routes, CLI) must add `api_key_id: None` to keep compiling.

- [ ] **Step 11: Add `ApiKeyRepository` to `DatabaseProvider` supertraits**

In `src/api/app.rs`:

```rust
use crate::db::repositories::api_keys::ApiKeyRepository;

pub trait DatabaseProvider:
    UserRepository
    + AdminUserRepository
    + SessionRepository
    + PromptRepository
    + CostRepository
    + BudgetRepository
    + AuditRepository
    + HookRepository
    + RateLimitRepository
    + ApiKeyRepository
    + Send
    + Sync
{
}

impl<T> DatabaseProvider for T where
    T: UserRepository
        + AdminUserRepository
        + SessionRepository
        + PromptRepository
        + CostRepository
        + BudgetRepository
        + AuditRepository
        + HookRepository
        + RateLimitRepository
        + ApiKeyRepository
        + Send
        + Sync
{
}
```

- [ ] **Step 12: Update `src/api/auth.rs` to check `api_keys` first**

Replace the body of `from_request_parts` with:

```rust
async fn from_request_parts(
    parts: &mut Parts,
    state: &AppState,
) -> Result<Self, Self::Rejection> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    use crate::db::repositories::users::UserRepository;

    let auth_header = parts
        .headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?;

    let key_hash = hash_token(auth_header);

    // 1. Try api_keys table first
    if let Some(api_key) = ApiKeyRepository::find_api_key_by_hash(&*state.db, &key_hash)
        .await
        .map_err(|_| ApiError::Internal)?
    {
        let mut user = UserRepository::find_by_id(&*state.db, api_key.user_id)
            .await
            .map_err(|_| ApiError::Internal)?
            .ok_or(ApiError::Unauthorized)?;

        if !user.enabled {
            return Err(ApiError::Unauthorized);
        }
        user.api_key_id = Some(api_key.id);
        return Ok(AuthenticatedUser(user));
    }

    // 2. Fall back to legacy users.api_key
    let user = state
        .db
        .find_by_api_key(&key_hash)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or(ApiError::Unauthorized)?;

    if !user.enabled {
        return Err(ApiError::Unauthorized);
    }
    Ok(AuthenticatedUser(user))
}
```

- [ ] **Step 13: Update `PolicyEngine::check` to include per-key rules**

In `src/router/policy.rs`, after loading user + group rules, add key-specific rules:

```rust
pub async fn check(&self, user: &User, model: &str) -> anyhow::Result<PolicyDecision> {
    use crate::db::repositories::budgets::BudgetRepository;
    use crate::db::repositories::costs::CostRepository;
    use crate::db::repositories::rate_limits::RateLimitRepository;

    let mut rules = BudgetRepository::list_for_user(&*self.db, user.id).await?;
    if let Some(ref group) = user.group_name {
        let group_rules = BudgetRepository::list_for_group(&*self.db, group).await?;
        rules.extend(group_rules);
    }
    // Per-key rules take precedence — check them first by prepending
    if let Some(key_id) = user.api_key_id {
        let key_rules = BudgetRepository::list_for_key(&*self.db, key_id).await?;
        rules = key_rules.into_iter().chain(rules).collect();
    }

    // ... rest of check unchanged ...
```

- [ ] **Step 14: Run all tests**

```bash
cargo test
```

Expected: all existing tests + new per-key tests pass.

- [ ] **Step 15: Add admin REST endpoints for API key management**

In `src/api/admin/routes.rs`, add four handlers:

```rust
// GET /admin/api/users/:id/keys — list API keys for user
pub async fn list_user_api_keys(
    State(state): State<AppState>,
    _admin: AdminJwt,
    Path(user_id): Path<i64>,
) -> Result<Json<Vec<crate::db::models::ApiKey>>, ApiError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    let keys = ApiKeyRepository::list_api_keys_for_user(&*state.db, user_id)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(Json(keys))
}

// POST /admin/api/users/:id/keys — create API key for user
#[derive(Deserialize)]
pub struct CreateApiKeyRequest {
    pub label: Option<String>,
}

pub async fn create_user_api_key(
    State(state): State<AppState>,
    _admin: AdminJwt,
    Path(user_id): Path<i64>,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    use crate::api::auth::hash_token;

    let raw_key = format!("mr-{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let key_hash = hash_token(&raw_key);

    let created = ApiKeyRepository::create_api_key(&*state.db, crate::db::models::NewApiKey {
        user_id,
        key_hash,
        label: body.label,
    })
    .await
    .map_err(|_| ApiError::Internal)?;

    // Return the raw key once — it cannot be recovered later
    Ok(Json(serde_json::json!({
        "id": created.id,
        "key": raw_key,
        "label": created.label,
        "created_at": created.created_at,
    })))
}

// DELETE /admin/api/keys/:id — revoke API key
pub async fn revoke_api_key(
    State(state): State<AppState>,
    _admin: AdminJwt,
    Path(key_id): Path<i64>,
) -> Result<axum::http::StatusCode, ApiError> {
    use crate::db::repositories::api_keys::ApiKeyRepository;
    ApiKeyRepository::revoke_api_key(&*state.db, key_id)
        .await
        .map_err(|_| ApiError::Internal)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
```

- [ ] **Step 16: Register new routes in `src/api/app.rs`**

In `build_router`, add:

```rust
use crate::api::admin::routes::{list_user_api_keys, create_user_api_key, revoke_api_key};

// After existing admin routes:
.route("/admin/api/users/:id/keys", get(list_user_api_keys).post(create_user_api_key))
.route("/admin/api/keys/:id/revoke", post(revoke_api_key))
```

- [ ] **Step 17: Run all tests**

```bash
cargo test
```

Expected: all tests pass.

- [ ] **Step 18: Commit**

```bash
git add migrations/002_per_key_budgets.sql \
        src/db/models.rs \
        src/db/repositories/api_keys.rs \
        src/db/repositories/mod.rs \
        src/db/repositories/budgets.rs \
        src/db/sqlite/api_keys.rs \
        src/db/sqlite/mod.rs \
        src/db/sqlite/budgets.rs \
        src/api/app.rs \
        src/api/auth.rs \
        src/router/policy.rs \
        src/api/admin/routes.rs \
        tests/test_per_key_budgets.rs
git commit -m "feat: add per-key API budgets — api_keys table, per-key budget rules, admin key management endpoints"
```
