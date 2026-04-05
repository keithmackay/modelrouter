# Phase 12: Quick Wins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement four independent medium-value, low-effort features: Anthropic `cache_control` passthrough for system messages, spend reset admin API, API key expiration, and concurrent request limits.

**Architecture:** Each task is fully independent. Tasks 1-3 are purely additive (new migration + endpoint or small code change). Task 4 adds a `ConcurrencyLimiter` to AppState and extends `PolicyDecision::Allow` to carry an optional concurrent request cap; all test files that construct `AppState` must be updated. The `User`, `ApiKey`, and `BudgetRule` models all use intermediate `*Row` structs for sqlx deserialization — any new column requires updating both the `Row` struct and its `From<Row>` mapping in `src/db/sqlite/`.

**Tech Stack:** Rust 2021, axum 0.7, sqlx 0.8 (SQLite + optional Postgres), `tokio::sync::Semaphore`, dashmap 6, existing repository + policy engine patterns

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `migrations/004_spend_reset.sql` | Create | `spend_reset_at` column on `users` |
| `migrations/005_api_key_expiry.sql` | Create | `expires_at` column on `api_keys` |
| `migrations/006_concurrent_limit.sql` | Create | `max_concurrent` column on `budget_rules` |
| `src/providers/anthropic.rs` | Modify | `translate_messages` — handle array-content system messages |
| `src/db/models.rs` | Modify | `spend_reset_at` on `User`; `expires_at` on `ApiKey`; `max_concurrent` on `BudgetRule` |
| `src/db/sqlite/users.rs` | Modify | Add `spend_reset_at` to `UserRow`, `From<UserRow>`, queries, and `reset_spend` impl |
| `src/db/sqlite/api_keys.rs` | Modify | Add `expires_at` to `ApiKeyRow`, `From<ApiKeyRow>`, all SELECT/INSERT statements |
| `src/db/sqlite/budgets.rs` | Modify | Add `max_concurrent` to `NewBudgetRule`, INSERT, and all 5 SELECT column lists (no Row intermediary) |
| `src/db/postgres/budgets.rs` | Modify | Same as SQLite — add `max_concurrent` to `NewBudgetRule`, INSERT/RETURNING, and all SELECT column lists (uses `$N` placeholders) |
| `src/db/repositories/users.rs` | Modify | Add `reset_spend` to `UserRepository` trait |
| `src/db/repositories/api_keys.rs` | Modify | Add `set_expiry` to `ApiKeyRepository` trait |
| `src/api/auth.rs` | Modify | Reject expired api_keys via `ApiKey::is_valid()` |
| `src/api/admin/routes.rs` | Modify | Add `POST /admin/api/users/:id/reset-spend` |
| `src/api/app.rs` | Modify | Register reset-spend route; add `concurrency` to AppState |
| `src/router/policy.rs` | Modify | Populate `max_concurrent` in `PolicyDecision::Allow` |
| `src/router/concurrency.rs` | Create | `ConcurrencyLimiter` using `DashMap<i64, Arc<Semaphore>>` |
| `src/router/mod.rs` | Modify | `pub mod concurrency;` |
| `src/api/routes/completions.rs` | Modify | Acquire concurrency permit after policy Allow |
| `src/api/routes/messages.rs` | Modify | Same concurrency check |
| `src/cli/mod.rs` | Modify | Construct `ConcurrencyLimiter` in AppState |
| `tests/test_anthropic_cache.rs` | Create | Unit tests for cache_control passthrough |
| `tests/test_key_expiry.rs` | Create | Unit tests for key expiry logic |
| `tests/test_concurrency.rs` | Create | Unit tests for ConcurrencyLimiter |
| All 8 test files with AppState literals | Modify | Add `concurrency` field (Task 4) |

---

### Task 1: Anthropic cache_control passthrough

**Files:**
- Modify: `src/providers/anthropic.rs`
- Create: `tests/test_anthropic_cache.rs`

**Context:** `translate_messages` extracts the system prompt by calling `m["content"].as_str()`, which returns `None` when content is a structured array like `[{"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}]`. This silently discards system messages whose content is an array — their text never reaches Anthropic.

User/assistant messages already pass through correctly (the `filtered` iterator calls `.cloned()` without touching content), so array content blocks on user/assistant messages already work today. The fix only needs to touch the system-prompt extraction path.

- [ ] **Step 1: Write the failing test**

Create `tests/test_anthropic_cache.rs`:
```rust
// tests/test_anthropic_cache.rs
use modelrouter::providers::anthropic::translate_messages;
use serde_json::json;

#[test]
fn string_system_message_still_works() {
    let messages = vec![
        json!({"role": "system", "content": "Be helpful."}),
        json!({"role": "user", "content": "Hi"}),
    ];
    let (system, filtered) = translate_messages(&messages);
    assert_eq!(system.as_deref(), Some("Be helpful."));
    assert_eq!(filtered.len(), 1);
}

#[test]
fn array_content_system_message_text_is_extracted() {
    // System messages with cache_control use array content blocks
    let messages = vec![
        json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "Be helpful.", "cache_control": {"type": "ephemeral"}}
            ]
        }),
        json!({"role": "user", "content": "Hi"}),
    ];
    let (system, filtered) = translate_messages(&messages);
    // The text should still be extracted even though content is an array
    assert_eq!(system.as_deref(), Some("Be helpful."));
    assert_eq!(filtered.len(), 1);
}

#[test]
fn array_content_user_message_preserved_as_array() {
    // User messages with cache_control content arrays already pass through
    let messages = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Hello", "cache_control": {"type": "ephemeral"}}
        ]
    })];
    let (system, filtered) = translate_messages(&messages);
    assert!(system.is_none());
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0]["content"].is_array(), "array content must be preserved");
    assert_eq!(filtered[0]["content"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn message_with_null_content_is_excluded() {
    let messages = vec![
        json!({"role": "user", "content": null}),
        json!({"role": "assistant", "content": "Hi"}),
    ];
    let (_, filtered) = translate_messages(&messages);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["role"], "assistant");
}
```

Run: `cargo test test_anthropic_cache 2>&1 | head -20`
Expected: `array_content_system_message_text_is_extracted` FAILS — array system content is currently silently dropped

- [ ] **Step 2: Fix `translate_messages` in `src/providers/anthropic.rs`**

Replace the system-prompt extraction (the `system_parts` block only):
```rust
// Before
let system_parts: Vec<String> = messages
    .iter()
    .filter_map(|m| {
        if m["role"].as_str() == Some("system") {
            m["content"].as_str().map(|s| s.to_string())
        } else {
            None
        }
    })
    .collect();
```

With:
```rust
// After — handle both string and array system content
let system_parts: Vec<String> = messages
    .iter()
    .filter_map(|m| {
        if m["role"].as_str() != Some("system") {
            return None;
        }
        if let Some(s) = m["content"].as_str() {
            // String content — common case
            return Some(s.to_string());
        }
        if let Some(arr) = m["content"].as_array() {
            // Array of content blocks (e.g. with cache_control) — extract text values
            let text = arr
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() { Some(text) } else { None }
        } else {
            None
        }
    })
    .collect();
```

Also add a null-content guard to the user/assistant filter:
```rust
let filtered: Vec<serde_json::Value> = messages
    .iter()
    .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
    .filter(|m| m["content"].is_string() || m["content"].is_array())
    .cloned()
    .collect();
```

- [ ] **Step 3: Run tests**

```bash
cargo test test_anthropic_cache 2>&1 | tail -10
cargo test 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add src/providers/anthropic.rs tests/test_anthropic_cache.rs
git commit -m "feat: handle array-content system messages for Anthropic cache_control"
```

---

### Task 2: Spend reset admin API

**Files:**
- Create: `migrations/004_spend_reset.sql`
- Modify: `src/db/models.rs`
- Modify: `src/db/sqlite/users.rs`
- Modify: `src/db/repositories/users.rs`
- Modify: `src/router/policy.rs`
- Modify: `src/api/admin/routes.rs`
- Modify: `src/api/app.rs`

**Context:** `UserRepository` uses a private `UserRow` struct in `src/db/sqlite/users.rs` for sqlx deserialization (see `src/db/sqlite/users.rs`). Adding `spend_reset_at` to the `User` model alone does nothing — the field must also be added to `UserRow` and its `From<UserRow>` mapping. All queries that SELECT from `users` must include the new column, or use `SELECT *` (check which pattern the existing queries use).

The `CostRepository` methods already accept a `since: &str` argument. The policy engine passes `window_start_for(&rule.window)` as `since`. To apply the spend reset, pass `max(window_start, user.spend_reset_at)` instead. No changes to the repository itself are needed.

- [ ] **Step 1: Create migration 004**

Create `migrations/004_spend_reset.sql`:
```sql
-- migrations/004_spend_reset.sql
-- Non-destructive spend reset: track the timestamp from which spend is counted.
-- NULL means count from the beginning of time (no reset performed).
ALTER TABLE users ADD COLUMN spend_reset_at TEXT;
```

- [ ] **Step 2: Add `spend_reset_at` to `User` model**

In `src/db/models.rs`, add to `User`:
```rust
    /// If set, only costs recorded after this timestamp count toward budget limits.
    /// Non-destructive: cost_ledger rows are not deleted.
    #[sqlx(default)]
    pub spend_reset_at: Option<String>,
```

- [ ] **Step 3: Add `spend_reset_at` to `UserRow` in `src/db/sqlite/users.rs`**

`UserRow` is the sqlx-deserialized intermediary — it MUST include the new column or queries will fail at runtime:
```rust
#[derive(sqlx::FromRow)]
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
    #[sqlx(default)]
    spend_reset_at: Option<String>,
}

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
            api_key_id: None,
            spend_reset_at: r.spend_reset_at,
        }
    }
}
```

The existing queries use explicit column lists like `SELECT id, name, api_key, ...`. Update each SELECT in `users.rs` to add `spend_reset_at` to the list. Alternatively, if the queries are short, switch to `SELECT *` — check the file and choose whichever is less invasive.

- [ ] **Step 4: Add `reset_spend` to `UserRepository` trait**

In `src/db/repositories/users.rs`:
```rust
/// Set spend_reset_at to the current UTC time, so future policy checks
/// only sum costs recorded after this point.
async fn reset_spend(&self, user_id: i64) -> anyhow::Result<()>;
```

Implement it in `src/db/sqlite/users.rs`:
```rust
async fn reset_spend(&self, user_id: i64) -> anyhow::Result<()> {
    let now = now_utc();
    sqlx::query("UPDATE users SET spend_reset_at = ? WHERE id = ?")
        .bind(&now)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
    Ok(())
}
```

- [ ] **Step 5: Apply spend reset in `src/router/policy.rs`**

In `check()`, find both places that call `CostRepository::sum_for_user_since` / `sum_tokens_for_user_since`. Replace `window_start_for(&rule.window)` with an effective start that respects `spend_reset_at`:

```rust
let raw_window_start = window_start_for(&rule.window);
// Honor spend_reset_at: use whichever timestamp is later
// Both are RFC3339 UTC strings; lexicographic comparison is correct for UTC
let window_start = match &user.spend_reset_at {
    Some(reset_at) if reset_at.as_str() > raw_window_start.as_str() => reset_at.clone(),
    _ => raw_window_start,
};
```

- [ ] **Step 6: Add `reset_user_spend` handler to `src/api/admin/routes.rs`**

```rust
/// POST /admin/api/users/:id/reset-spend
/// Sets spend_reset_at to now. Future policy budget checks ignore prior spend.
pub async fn reset_user_spend(
    State(state): State<AppState>,
    _claims: AdminClaims,
    Path(user_id): Path<i64>,
) -> Result<axum::Json<serde_json::Value>, ApiError> {
    use crate::db::repositories::users::UserRepository;
    state.db.reset_spend(user_id).await.map_err(|_| ApiError::Internal)?;
    Ok(axum::Json(serde_json::json!({ "user_id": user_id, "reset": true })))
}
```

- [ ] **Step 7: Register route in `src/api/app.rs`**

In `build_router`, add:
```rust
.route("/admin/api/users/:id/reset-spend", post(reset_user_spend))
```

Import `reset_user_spend` in the admin routes use block.

- [ ] **Step 8: Run tests**

```bash
cargo test 2>&1 | tail -10
cargo test --features otel 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 9: Commit**

```bash
git add migrations/004_spend_reset.sql src/db/models.rs src/db/sqlite/users.rs \
        src/db/repositories/users.rs src/router/policy.rs \
        src/api/admin/routes.rs src/api/app.rs
git commit -m "feat: add spend reset admin API with non-destructive spend_reset_at"
```

---

### Task 3: API key expiration

**Files:**
- Create: `migrations/005_api_key_expiry.sql`
- Modify: `src/db/models.rs`
- Modify: `src/db/sqlite/api_keys.rs`
- Modify: `src/db/repositories/api_keys.rs`
- Modify: `src/api/auth.rs`
- Create: `tests/test_key_expiry.rs`

**Context:** `ApiKey` is deserialized via `ApiKeyRow` in `src/db/sqlite/api_keys.rs` — the same intermediate row pattern as `UserRow`. All four methods in that file use explicit `SELECT` column lists and an explicit `INSERT` column list. Every one of them must be updated to include `expires_at`.

- [ ] **Step 1: Write the failing test**

Create `tests/test_key_expiry.rs`:
```rust
// tests/test_key_expiry.rs
use modelrouter::db::models::ApiKey;

fn make_key(expires_at: Option<String>) -> ApiKey {
    ApiKey {
        id: 1,
        user_id: 1,
        key_hash: "abc".to_string(),
        label: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        expires_at,
    }
}

#[test]
fn key_without_expiry_is_valid() {
    assert!(make_key(None).is_valid());
}

#[test]
fn key_with_future_expiry_is_valid() {
    // Use a far-future date so this test never fails due to time passing
    let future = "2099-12-31T23:59:59+00:00".to_string();
    assert!(make_key(Some(future)).is_valid());
}

#[test]
fn key_with_past_expiry_is_expired() {
    let past = "2020-01-01T00:00:00+00:00".to_string();
    assert!(!make_key(Some(past)).is_valid());
}

#[test]
fn disabled_key_is_invalid_regardless_of_expiry() {
    let mut key = make_key(None);
    key.enabled = false;
    assert!(!key.is_valid());
}
```

Run: `cargo test test_key_expiry 2>&1 | head -20`
Expected: compile error — `ApiKey` has no field `expires_at`

- [ ] **Step 2: Create migration 005**

Create `migrations/005_api_key_expiry.sql`:
```sql
-- migrations/005_api_key_expiry.sql
ALTER TABLE api_keys ADD COLUMN expires_at TEXT;
```

- [ ] **Step 3: Update `ApiKey` model in `src/db/models.rs`**

Add field and `is_valid()` method:
```rust
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: i64,
    pub user_id: i64,
    pub key_hash: String,
    pub label: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    /// RFC3339 UTC expiry. None = never expires.
    #[sqlx(default)]
    pub expires_at: Option<String>,
}

impl ApiKey {
    /// Returns true if the key is enabled and not past its expiry.
    /// Both timestamps are RFC3339 UTC strings; lexicographic comparison is correct.
    pub fn is_valid(&self) -> bool {
        if !self.enabled {
            return false;
        }
        match &self.expires_at {
            None => true,
            Some(exp) => exp.as_str() > chrono::Utc::now().to_rfc3339().as_str(),
        }
    }
}
```

Also add `expires_at: Option<String>` to `NewApiKey`:
```rust
pub struct NewApiKey {
    pub user_id: i64,
    pub key_hash: String,
    pub label: Option<String>,
    pub expires_at: Option<String>,
}
```

- [ ] **Step 4: Update `ApiKeyRow` and all queries in `src/db/sqlite/api_keys.rs`**

The `ApiKeyRow` struct is the sqlx intermediary. The `#[sqlx(default)]` annotation on `ApiKey` is **inert** here because `ApiKey` itself is not used with `query_as`. Update `ApiKeyRow`:

```rust
#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: i64,
    user_id: i64,
    key_hash: String,
    label: Option<String>,
    enabled: i64,
    created_at: String,
    #[sqlx(default)]
    expires_at: Option<String>,
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
            expires_at: r.expires_at,
        }
    }
}
```

Update **all four** SQL statements in the `ApiKeyRepository` impl:
- `find_api_key_by_hash`: add `expires_at` to SELECT
- `list_api_keys_for_user`: add `expires_at` to SELECT
- `create_api_key` INSERT: add `expires_at` to column list and bind
- `create_api_key` SELECT-after-insert: add `expires_at` to SELECT

Updated `create_api_key`:
```rust
async fn create_api_key(&self, key: NewApiKey) -> anyhow::Result<ApiKey> {
    let now = now_utc();
    let result = sqlx::query(
        "INSERT INTO api_keys (user_id, key_hash, label, enabled, created_at, expires_at) \
         VALUES (?, ?, ?, 1, ?, ?)"
    )
    .bind(key.user_id)
    .bind(&key.key_hash)
    .bind(&key.label)
    .bind(&now)
    .bind(&key.expires_at)
    .execute(&self.pool)
    .await?;

    let id = result.last_insert_rowid();
    let row = sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, user_id, key_hash, label, enabled, created_at, expires_at \
         FROM api_keys WHERE id = ?"
    )
    .bind(id)
    .fetch_one(&self.pool)
    .await?;
    Ok(ApiKey::from(row))
}
```

- [ ] **Step 5: Update `auth.rs` to use `is_valid()`**

In `src/api/auth.rs`, the api_keys path currently checks only `user.enabled`. Change it to also check key validity:

```rust
// Before
if !user.enabled {
    return Err(ApiError::Unauthorized);
}
user.api_key_id = Some(api_key.id);
return Ok(AuthenticatedUser(user));

// After
if !api_key.is_valid() || !user.enabled {
    return Err(ApiError::Unauthorized);
}
user.api_key_id = Some(api_key.id);
return Ok(AuthenticatedUser(user));
```

- [ ] **Step 6: Fix `NewApiKey` call sites**

Search for all `NewApiKey {` struct literals:
```bash
grep -rn "NewApiKey {" src/ tests/
```

Add `expires_at: None` to each one that doesn't already have it.

- [ ] **Step 7: Run tests**

```bash
cargo test test_key_expiry 2>&1 | tail -10
cargo test 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 8: Commit**

```bash
git add migrations/005_api_key_expiry.sql src/db/models.rs src/db/sqlite/api_keys.rs \
        src/api/auth.rs tests/test_key_expiry.rs
git commit -m "feat: add API key expiration (expires_at) with auth enforcement"
```

---

### Task 4: Concurrent request limits

**Files:**
- Create: `migrations/006_concurrent_limit.sql`
- Create: `src/router/concurrency.rs`
- Modify: `src/router/mod.rs`
- Modify: `src/router/policy.rs`
- Modify: `src/db/models.rs` (add `max_concurrent` to `BudgetRule`)
- Modify: `src/db/sqlite/budgets.rs` (add `max_concurrent` to SELECT/INSERT — no BudgetRuleRow, `BudgetRule` maps directly)
- Modify: `src/api/app.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `src/api/routes/messages.rs`
- Modify: `src/cli/mod.rs`
- Modify: ALL 8 test files with `AppState {` literals
- Create: `tests/test_concurrency.rs`

**Context:** `PolicyDecision::Allow` currently has no fields. We change it to a struct variant carrying `max_concurrent: Option<u32>`. This breaks all existing match arms — update them. `ApiError` has no `TooManyRequests` variant; use `ApiError::PolicyDenied { reason: "...", status: 429 }`.

Unlike `users.rs` and `api_keys.rs`, `src/db/sqlite/budgets.rs` uses `sqlx::query_as::<_, BudgetRule>(...)` directly — there is **no `BudgetRuleRow`** intermediary. `BudgetRule` maps directly from sqlx, so `#[sqlx(default)]` on that struct is effective.

- [ ] **Step 1: Write failing unit tests**

Create `tests/test_concurrency.rs`:
```rust
// tests/test_concurrency.rs

#[test]
fn limiter_allows_requests_under_limit() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    assert!(limiter.try_acquire(1, 2).is_some(), "first should succeed");
    assert!(limiter.try_acquire(1, 2).is_some(), "second should succeed");
}

#[test]
fn limiter_denies_when_at_capacity() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    let _p1 = limiter.try_acquire(1, 1).expect("first should succeed");
    assert!(limiter.try_acquire(1, 1).is_none(), "second should be denied");
}

#[test]
fn permit_drop_releases_slot() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    { let _p = limiter.try_acquire(1, 1).expect("first"); }
    assert!(limiter.try_acquire(1, 1).is_some(), "slot available after drop");
}

#[test]
fn users_tracked_independently() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    let _p1 = limiter.try_acquire(1, 1).expect("user 1");
    assert!(limiter.try_acquire(2, 1).is_some(), "user 2 is independent");
}

#[test]
fn max_zero_denies_all() {
    let limiter = modelrouter::router::concurrency::ConcurrencyLimiter::new();
    assert!(limiter.try_acquire(1, 0).is_none());
}
```

Run: `cargo test test_concurrency 2>&1 | head -10`
Expected: compile error — `concurrency` module not found

- [ ] **Step 2: Create migration 006**

Create `migrations/006_concurrent_limit.sql`:
```sql
-- migrations/006_concurrent_limit.sql
ALTER TABLE budget_rules ADD COLUMN max_concurrent INTEGER;
```

- [ ] **Step 3: Create `src/router/concurrency.rs`**

```rust
// src/router/concurrency.rs
//
// Per-user concurrency limiter using DashMap<user_id, Arc<Semaphore>>.
// Semaphores are created lazily on first use. The capacity is fixed at the
// value passed on first call for each user_id — if the budget rule changes,
// the new limit takes effect only after a process restart. This is a known
// limitation acceptable for v1.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct ConcurrencyLimiter {
    semaphores: DashMap<i64, Arc<Semaphore>>,
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self { semaphores: DashMap::new() }
    }

    /// Try to acquire a slot for `user_id` with capacity `max`.
    ///
    /// Returns `Some(permit)` if a slot was available, `None` if at capacity
    /// or `max` is 0. Hold the returned permit for the duration of the upstream
    /// call — dropping it releases the slot.
    pub fn try_acquire(&self, user_id: i64, max: u32) -> Option<OwnedSemaphorePermit> {
        if max == 0 {
            return None;
        }
        let semaphore = self
            .semaphores
            .entry(user_id)
            .or_insert_with(|| Arc::new(Semaphore::new(max as usize)))
            .clone();
        semaphore.try_acquire_owned().ok()
    }
}

impl Default for ConcurrencyLimiter {
    fn default() -> Self { Self::new() }
}
```

- [ ] **Step 4: Add module declaration to `src/router/mod.rs`**

```rust
pub mod concurrency;
```

- [ ] **Step 5: Run concurrency unit tests**

```bash
cargo test test_concurrency 2>&1 | tail -10
```
Expected: all 5 pass

- [ ] **Step 6: Add `max_concurrent` to `BudgetRule` and its Row intermediary**

In `src/db/models.rs`, find `BudgetRule` and add:
```rust
    #[sqlx(default)]
    pub max_concurrent: Option<i64>,
```

`src/db/sqlite/budgets.rs` maps directly to `BudgetRule` (no `BudgetRuleRow` intermediary), so `#[sqlx(default)]` on `BudgetRule` is sufficient for sqlx to handle NULL from existing rows. However, you must also update:
- The `NewBudgetRule` struct in `src/db/models.rs`: add `pub max_concurrent: Option<i64>`
- The `create()` INSERT in `budgets.rs`: add `max_concurrent` to the column list and bind it
- All five SELECT statements in `budgets.rs` that use explicit column lists: add `max_concurrent` to each

This ensures budget rules with a concurrent limit can actually be created via the admin API. Without updating `NewBudgetRule` and the INSERT, the column will always be NULL regardless of what the caller sends.

Also update `src/db/postgres/budgets.rs` with the same changes. It follows the same direct-mapping pattern (no BudgetRuleRow) but uses `$1`-style placeholders instead of `?`. Add `max_concurrent` to the INSERT column list, the `RETURNING` clause (if used), and all SELECT column lists in that file.

- [ ] **Step 7: Change `PolicyDecision::Allow` to carry `max_concurrent`**

In `src/router/policy.rs`:
```rust
pub enum PolicyDecision {
    Allow {
        /// Most restrictive max_concurrent across all applicable budget rules.
        /// None means unlimited.
        max_concurrent: Option<u32>,
    },
    Deny {
        reason: String,
        status: u16,
        budget_context: Option<BudgetContext>,
    },
}
```

In the `check()` method:
- Add before the rule loop: `let mut min_concurrent: Option<u32> = None;`
- Inside the rule loop, after the existing checks, add:
  ```rust
  if let Some(mc) = rule.max_concurrent {
      let mc = mc.max(0) as u32;
      min_concurrent = Some(min_concurrent.map_or(mc, |prev| prev.min(mc)));
  }
  ```
- Change the final return from `Ok(PolicyDecision::Allow)` to:
  ```rust
  span.record("policy.result", "allow");
  Ok(PolicyDecision::Allow { max_concurrent: min_concurrent })
  ```

- [ ] **Step 8: Update all `PolicyDecision::Allow` match arms**

Run:
```bash
grep -rn "PolicyDecision::Allow" src/
```

For each match arm `PolicyDecision::Allow => { ... }`, change to `PolicyDecision::Allow { max_concurrent } => { ... }` (or `PolicyDecision::Allow { .. }` if max_concurrent is unused in that arm).

- [ ] **Step 9: Add concurrency check to `completions.rs`**

In the `PolicyDecision::Allow { max_concurrent }` arm, before the upstream provider call:
```rust
// Acquire a concurrency slot if a limit is configured for this user
let _concurrency_permit = if let Some(max) = max_concurrent {
    match state.concurrency.try_acquire(user.id, max) {
        Some(permit) => Some(permit),
        None => return Err(ApiError::PolicyDenied {
            reason: "concurrent request limit exceeded".to_string(),
            status: 429,
        }),
    }
} else {
    None
};
// _concurrency_permit is held until this function returns (RAII)
```

Apply the same pattern in `messages.rs`.

- [ ] **Step 10: Add `concurrency` to AppState**

In `src/api/app.rs`:
```rust
pub concurrency: Arc<crate::router::concurrency::ConcurrencyLimiter>,
```

In `src/cli/mod.rs`, find the `AppState { ... }` construction and add:
```rust
concurrency: Arc::new(crate::router::concurrency::ConcurrencyLimiter::new()),
```

- [ ] **Step 11: Update ALL 8 test files with AppState literals**

These files ALL contain `AppState {` struct literals (confirmed by grep):
- `tests/test_completions.rs`
- `tests/test_cache.rs`
- `tests/test_embeddings.rs`
- `tests/test_messages.rs`
- `tests/test_per_key_budgets.rs`
- `tests/test_dashboard.rs`
- `tests/test_prometheus.rs`
- `tests/test_telemetry.rs` — gated `#![cfg(feature = "otel")]`

Add to each `AppState { ... }` literal:
```rust
concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
```

- [ ] **Step 12: Run full test suite**

```bash
cargo test 2>&1 | tail -15
cargo test --features otel 2>&1 | tail -10
cargo test --features bedrock 2>&1 | tail -10
```
Expected: all pass in all three configurations

- [ ] **Step 13: Commit**

```bash
git add migrations/006_concurrent_limit.sql \
        src/router/concurrency.rs src/router/mod.rs src/router/policy.rs \
        src/db/models.rs src/db/sqlite/budgets.rs src/db/postgres/budgets.rs \
        src/api/app.rs src/api/routes/completions.rs src/api/routes/messages.rs \
        src/cli/mod.rs \
        tests/test_concurrency.rs \
        tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs \
        tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs \
        tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: add per-user concurrent request limits via semaphore"
```

---

## Common Pitfalls

1. **`*Row` intermediary structs** — `User`, `ApiKey`, and `BudgetRule` all use private `*Row` structs for sqlx deserialization in `src/db/sqlite/`. Adding a field to the public model struct does NOT make sqlx deserialize it. Always update the `*Row` struct AND its `From<*Row>` mapping AND the SELECT statements.

2. **`test_telemetry.rs` is gated by `#[cfg(feature = "otel")]`** — plain `cargo test` passes but `cargo test --features otel` fails if AppState fields are missing. Always run both.

3. **`PolicyDecision::Allow` is now a struct variant** — update all match arms to `PolicyDecision::Allow { .. }` or `PolicyDecision::Allow { max_concurrent }`. The unit variant syntax `PolicyDecision::Allow` without braces will not compile.

4. **`ApiError` has no `TooManyRequests` variant** — use `ApiError::PolicyDenied { reason: "...", status: 429 }`. This is the existing pattern used by rate limits and budget denials.

5. **Semaphore capacity is fixed at first-use** — `ConcurrencyLimiter` creates the semaphore with the `max` value from the first `try_acquire` call for each `user_id`. If the budget rule changes, the new limit only applies after a process restart. Documented in the code comment.

6. **RFC3339 lexicographic comparison requires consistent UTC representation** — `chrono::Utc::now().to_rfc3339()` emits `+00:00` suffix (not `Z`). Timestamps stored in the database use the same function, so comparisons are consistent within this codebase. Do not mix timestamps from external sources that use `Z` suffix without normalizing first.

7. **`BudgetRule.max_concurrent` needs `#[sqlx(default)]`** — without it, existing rows that have `NULL` in the new column will cause a deserialization error. Same for `User.spend_reset_at` and `ApiKey.expires_at` in their respective `*Row` structs.
