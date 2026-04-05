# Phase 13: Reliability and Security Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add three independent features: a per-provider circuit breaker for fast-fail on degraded upstreams, in-memory IP-based rate limiting as pre-auth abuse protection, and per-tag budgets that allow budget rules to target a tag assigned to an API key.

**Architecture:** Each task is fully independent. Task 1 (circuit breaker) adds a `CircuitBreaker` struct to `src/router/` and wires it into the existing provider dispatch loop in `completions.rs` and `messages.rs`. Task 2 (IP rate limiting) adds an axum middleware layer in `src/api/middleware/` that runs before auth using in-memory `DashMap` counters. Task 3 (per-tag budgets) adds a `tag` column to both `api_keys` and `budget_rules`, populates `user.api_key_tag` in the auth extractor, and extends the policy engine to match tag-based rules.

**Tech Stack:** Rust 2021, axum 0.7, tokio, dashmap 6, `std::sync::Mutex`, `std::time::Instant`, sqlx 0.8

**Note on Groq / Mistral / DeepSeek / OpenRouter:** These providers already work today — any provider not explicitly named in `registry.rs` falls back to `OpenAICompatAdapter`. Operators just need to set `providers.groq.api_key` and `providers.groq.api_base = "https://api.groq.com/openai/v1"` in config. No code changes are needed.

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `src/router/circuit_breaker.rs` | Create | `CircuitBreaker` struct, `is_open()`, `record_success()`, `record_failure()` |
| `src/router/mod.rs` | Modify | `pub mod circuit_breaker;` |
| `src/api/app.rs` | Modify | Add `circuit_breaker` field to `AppState` (Tasks 1 and 2) |
| `src/api/routes/completions.rs` | Modify | Circuit breaker check + IP middleware |
| `src/api/routes/messages.rs` | Modify | Same circuit breaker check |
| `src/cli/mod.rs` | Modify | Wire `CircuitBreaker` into AppState; change `axum::serve` to use `into_make_service_with_connect_info` |
| `src/api/middleware/mod.rs` | Create | `pub mod ip_rate_limit;` |
| `src/api/middleware/ip_rate_limit.rs` | Create | `IpRateLimiter` struct + axum middleware fn |
| `src/api/mod.rs` | Modify | `pub mod middleware;` |
| `migrations/007_api_key_tag.sql` | Create | `tag TEXT` column on `api_keys` |
| `migrations/008_budget_rule_tag.sql` | Create | `tag TEXT` column on `budget_rules` |
| `src/db/models.rs` | Modify | `tag` on `ApiKey`; `tag` on `BudgetRule`; `api_key_tag` on `User` (in-memory only) |
| `src/db/sqlite/api_keys.rs` | Modify | `ApiKeyRow.tag`, `From<ApiKeyRow>`, all SQL statements |
| `src/db/sqlite/budgets.rs` | Modify | `BudgetRule.tag` in all SELECTs + INSERT (no BudgetRuleRow) |
| `src/db/postgres/budgets.rs` | Modify | Same as sqlite/budgets.rs with `$N` placeholders |
| `src/db/repositories/budgets.rs` | Modify | Add `list_for_tag` to `BudgetRepository` trait |
| `src/db/sqlite/budgets.rs` | Modify | Implement `list_for_tag` |
| `src/api/auth.rs` | Modify | Populate `user.api_key_tag` from the looked-up `ApiKey.tag` |
| `src/router/policy.rs` | Modify | Include tag-matched budget rules in `check()` |
| `tests/test_circuit_breaker.rs` | Create | Unit tests for circuit breaker state machine |
| `tests/test_ip_rate_limit.rs` | Create | Unit tests for IpRateLimiter |
| All 8 AppState test files | Modify | Add `circuit_breaker` field (Task 1) |

---

### Task 1: Circuit Breaker

**Files:**
- Create: `src/router/circuit_breaker.rs`
- Modify: `src/router/mod.rs`
- Modify: `src/api/app.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `src/api/routes/messages.rs`
- Modify: `src/cli/mod.rs`
- Modify: all 8 test files with `AppState {` literals
- Create: `tests/test_circuit_breaker.rs`

**Context:** The provider dispatch loop in `completions.rs` (lines ~209-240) currently tries `adapter.complete()`, catches errors, and falls through to the fallback chain. A circuit breaker sits in front of `adapter.complete()`: if the provider has had >= N failures in the recent window, skip it immediately and treat it as a failure (triggering the fallback chain). The circuit breaker has 3 states: **Closed** (normal), **Open** (skip this provider), **HalfOpen** (cooldown elapsed, try one probe request).

Transitions:
- Closed → Open: when `failure_count >= threshold`
- Open → HalfOpen: when `elapsed since last_failure >= cooldown`
- HalfOpen → Closed: on success (reset count)
- HalfOpen → Open: on failure

`is_open()` is the read method — it also performs the Open → HalfOpen transition when the cooldown has elapsed (atomic: hold the lock during check).

- [ ] **Step 1: Write failing unit tests**

Create `tests/test_circuit_breaker.rs`:
```rust
// tests/test_circuit_breaker.rs
use modelrouter::router::circuit_breaker::CircuitBreaker;

#[test]
fn new_circuit_starts_closed() {
    let cb = CircuitBreaker::new(3, 60);
    assert!(!cb.is_open("openai"), "new circuit must be closed");
}

#[test]
fn circuit_opens_after_threshold_failures() {
    let cb = CircuitBreaker::new(3, 60);
    cb.record_failure("openai");
    cb.record_failure("openai");
    assert!(!cb.is_open("openai"), "still closed before threshold");
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "must be open after 3 failures");
}

#[test]
fn success_resets_failure_count() {
    let cb = CircuitBreaker::new(3, 60);
    cb.record_failure("openai");
    cb.record_failure("openai");
    cb.record_success("openai");
    cb.record_failure("openai");
    cb.record_failure("openai");
    assert!(!cb.is_open("openai"), "counter resets on success, 2 failures not enough");
}

#[test]
fn open_circuit_transitions_to_half_open_after_zero_cooldown() {
    // Use cooldown_secs=0 so elapsed >= cooldown immediately
    let cb = CircuitBreaker::new(1, 0);
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "open after 1 failure");
    // With 0s cooldown, second call to is_open should see HalfOpen and return false
    assert!(!cb.is_open("openai"), "half-open after cooldown elapsed");
}

#[test]
fn half_open_closes_on_success() {
    let cb = CircuitBreaker::new(1, 0);
    cb.record_failure("openai");
    assert!(cb.is_open("openai"));  // open
    assert!(!cb.is_open("openai")); // half-open
    cb.record_success("openai");
    assert!(!cb.is_open("openai"), "closed after success in half-open");
}

#[test]
fn half_open_reopens_on_failure() {
    let cb = CircuitBreaker::new(1, 0);
    cb.record_failure("openai");
    assert!(cb.is_open("openai"));  // open
    assert!(!cb.is_open("openai")); // half-open
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "re-opened after failure in half-open");
}

#[test]
fn providers_tracked_independently() {
    let cb = CircuitBreaker::new(2, 60);
    cb.record_failure("openai");
    cb.record_failure("openai");
    assert!(cb.is_open("openai"), "openai open");
    assert!(!cb.is_open("anthropic"), "anthropic unaffected");
}
```

Run: `cargo test test_circuit_breaker 2>&1 | head -10`
Expected: compile error — `circuit_breaker` module not found

- [ ] **Step 2: Create `src/router/circuit_breaker.rs`**

```rust
// src/router/circuit_breaker.rs
//
// Per-provider circuit breaker using a 3-state state machine:
// Closed (normal) → Open (fast-fail) → HalfOpen (probe) → Closed
//
// Capacity is tracked per provider name. Thread-safe via Mutex per entry.

use dashmap::DashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

struct ProviderCircuit {
    state: CircuitState,
    failure_count: u32,
    last_failure: Option<Instant>,
}

pub struct CircuitBreaker {
    circuits: DashMap<String, Mutex<ProviderCircuit>>,
    failure_threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, cooldown_secs: u64) -> Self {
        Self {
            circuits: DashMap::new(),
            failure_threshold,
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    /// Returns true if requests to this provider should be fast-failed.
    /// Also performs the Open → HalfOpen transition when the cooldown has elapsed.
    pub fn is_open(&self, provider: &str) -> bool {
        let entry = self.circuits
            .entry(provider.to_string())
            .or_insert_with(|| Mutex::new(ProviderCircuit {
                state: CircuitState::Closed,
                failure_count: 0,
                last_failure: None,
            }));
        let mut circuit = entry.lock().unwrap();

        match circuit.state {
            CircuitState::Closed | CircuitState::HalfOpen => false,
            CircuitState::Open => {
                // Check if cooldown has elapsed → transition to HalfOpen
                if let Some(last) = circuit.last_failure {
                    if last.elapsed() >= self.cooldown {
                        circuit.state = CircuitState::HalfOpen;
                        return false; // Allow one probe request
                    }
                }
                true // Still open
            }
        }
    }

    /// Record a successful provider response.
    pub fn record_success(&self, provider: &str) {
        if let Some(entry) = self.circuits.get(provider) {
            let mut circuit = entry.lock().unwrap();
            circuit.state = CircuitState::Closed;
            circuit.failure_count = 0;
        }
    }

    /// Record a failed provider response.
    pub fn record_failure(&self, provider: &str) {
        let entry = self.circuits
            .entry(provider.to_string())
            .or_insert_with(|| Mutex::new(ProviderCircuit {
                state: CircuitState::Closed,
                failure_count: 0,
                last_failure: None,
            }));
        let mut circuit = entry.lock().unwrap();
        circuit.last_failure = Some(Instant::now());
        match circuit.state {
            CircuitState::Closed => {
                circuit.failure_count += 1;
                if circuit.failure_count >= self.failure_threshold {
                    circuit.state = CircuitState::Open;
                }
            }
            CircuitState::HalfOpen | CircuitState::Open => {
                circuit.state = CircuitState::Open;
                circuit.failure_count = 1; // Reset so threshold is consistent after re-open
            }
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(5, 60)
    }
}
```

- [ ] **Step 3: Add module declaration to `src/router/mod.rs`**

Add: `pub mod circuit_breaker;`

- [ ] **Step 4: Run unit tests**

```bash
cargo test test_circuit_breaker 2>&1 | tail -10
```
Expected: all 7 pass

- [ ] **Step 5: Add `circuit_breaker` to AppState**

In `src/api/app.rs`, add field to `AppState`:
```rust
pub circuit_breaker: Arc<crate::router::circuit_breaker::CircuitBreaker>,
```

In `src/cli/mod.rs`, add to `AppState { ... }`:
```rust
circuit_breaker: Arc::new(crate::router::circuit_breaker::CircuitBreaker::default()),
```

- [ ] **Step 6: Wire circuit breaker into `completions.rs` dispatch loop**

Read `src/api/routes/completions.rs` first. Find the `loop` block that calls `adapter.complete()`. Before `adapter.complete()`, add the open check; after the result, call record:

```rust
let result = loop {
    // Circuit breaker: fast-fail if provider is degraded
    if state.circuit_breaker.is_open(&current_provider) {
        tracing::warn!(provider = current_provider.as_str(), "circuit breaker open, skipping provider");
        let pseudo_err = anyhow::anyhow!("circuit breaker open for {}", current_provider);
        if let Some(next_model) = state.fallback.next_after(&current_model) {
            let (next_provider, next_canonical) = state.router.resolve(next_model);
            current_model = next_canonical;
            current_provider = next_provider;
            continue;
        } else {
            return Err(ApiError::ProviderError(pseudo_err));
        }
    }

    let adapter = state
        .provider_registry
        .get(&current_provider)
        .map_err(ApiError::ProviderError)?;
    match adapter.complete(&build_normalized_request(&body, current_model.clone()))
        .instrument(...)
        .await
    {
        Ok(r) => {
            state.circuit_breaker.record_success(&current_provider);
            break r;
        }
        Err(e) => {
            state.circuit_breaker.record_failure(&current_provider);
            tracing::warn!(...);
            if let Some(next_model) = state.fallback.next_after(&current_model) {
                ...
            } else {
                return Err(ApiError::ProviderError(e));
            }
        }
    }
};
```

Apply the same pattern to the streaming dispatch in `completions.rs` and to both dispatch loops in `messages.rs`.

- [ ] **Step 7: Update all 8 test files**

These files all contain `AppState {` literals:
- `tests/test_completions.rs`
- `tests/test_cache.rs`
- `tests/test_embeddings.rs`
- `tests/test_messages.rs`
- `tests/test_per_key_budgets.rs`
- `tests/test_dashboard.rs`
- `tests/test_prometheus.rs`
- `tests/test_telemetry.rs` (gated `#[cfg(feature = "otel")]`)

Add to each:
```rust
circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
```

- [ ] **Step 8: Run full test suite**

```bash
cargo test 2>&1 | tail -15
cargo test --features otel 2>&1 | tail -10
cargo test --features bedrock 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 9: Commit**

```bash
git add src/router/circuit_breaker.rs src/router/mod.rs \
        src/api/app.rs src/cli/mod.rs \
        src/api/routes/completions.rs src/api/routes/messages.rs \
        tests/test_circuit_breaker.rs \
        tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs \
        tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs \
        tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: add per-provider circuit breaker for fast-fail on degraded upstreams"
```

---

### Task 2: IP Rate Limiting

**Files:**
- Create: `src/api/middleware/ip_rate_limit.rs`
- Create: `src/api/middleware/mod.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/api/app.rs` (add `ip_rate_limiter` to AppState, apply middleware)
- Modify: `src/cli/mod.rs` (change `axum::serve` to use `into_make_service_with_connect_info`)
- Modify: `src/config/schema.rs` (add `ip_rate_limit_rpm` to `ServerConfig`)
- Create: `tests/test_ip_rate_limit.rs`

**Context:** IP rate limiting protects against pre-auth abuse and DDoS amplification. It runs as axum middleware BEFORE the auth extractor. The rate limiter is in-memory (`DashMap`) — counters reset on restart, which is acceptable for abuse prevention.

To get client IP in axum middleware, the server must use `.into_make_service_with_connect_info::<SocketAddr>()` (instead of `.into_make_service()`). The middleware reads `ConnectInfo<SocketAddr>` from request extensions. If `ip_rate_limit_rpm` is 0 (the default), the middleware is a no-op — no overhead for deployments that don't need it.

**Important:** The middleware is the LAST `.layer()` call in `build_router` so it runs FIRST (axum layers execute in LIFO order).

- [ ] **Step 1: Write failing unit tests**

Create `tests/test_ip_rate_limit.rs`:
```rust
// tests/test_ip_rate_limit.rs
use modelrouter::api::middleware::ip_rate_limit::IpRateLimiter;

#[test]
fn allows_requests_under_limit() {
    let limiter = IpRateLimiter::new(3);
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(limiter.check_and_increment("1.2.3.4"));
}

#[test]
fn denies_requests_over_limit() {
    let limiter = IpRateLimiter::new(2);
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(!limiter.check_and_increment("1.2.3.4"), "third request should be denied");
}

#[test]
fn ips_tracked_independently() {
    let limiter = IpRateLimiter::new(1);
    assert!(limiter.check_and_increment("1.2.3.4"));
    assert!(!limiter.check_and_increment("1.2.3.4"), "1.2.3.4 at limit");
    assert!(limiter.check_and_increment("5.6.7.8"), "5.6.7.8 unaffected");
}

#[test]
fn zero_limit_disables_limiting() {
    // limit=0 means disabled — all requests pass
    let limiter = IpRateLimiter::new(0);
    for _ in 0..100 {
        assert!(limiter.check_and_increment("1.2.3.4"));
    }
}
```

Run: `cargo test test_ip_rate_limit 2>&1 | head -10`
Expected: compile error — `middleware` module not found

- [ ] **Step 2: Add `ip_rate_limit_rpm` to `ServerConfig`**

Read `src/config/schema.rs`. In `ServerConfig`, add:
```rust
/// Max requests per minute per IP address. 0 = disabled (default).
#[serde(default)]
pub ip_rate_limit_rpm: u32,
```

Also add it to `Default for ServerConfig`:
```rust
ip_rate_limit_rpm: 0,
```

- [ ] **Step 3: Create `src/api/middleware/ip_rate_limit.rs`**

```rust
// src/api/middleware/ip_rate_limit.rs
//
// In-memory per-IP rate limiter. Counters are keyed by (ip, minute_bucket)
// and live in a DashMap. Stale keys from prior minutes are not evicted
// automatically — the map grows at most one entry per unique IP per minute
// and old entries are naturally ignored (they cannot cause over-counting
// because the current minute's key will be different).

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Shared rate limiter state stored in AppState.
pub struct IpRateLimiter {
    /// (ip_address, minute_bucket) → request count
    counts: DashMap<(String, String), u64>,
    /// Max requests per minute per IP. 0 = disabled.
    limit_rpm: u32,
}

impl IpRateLimiter {
    pub fn new(limit_rpm: u32) -> Self {
        Self { counts: DashMap::new(), limit_rpm }
    }

    /// Increment the counter for `ip` in the current minute window.
    /// Returns true if the request is allowed, false if the limit is exceeded.
    pub fn check_and_increment(&self, ip: &str) -> bool {
        if self.limit_rpm == 0 {
            return true; // Disabled
        }
        let bucket = current_minute_bucket();
        let key = (ip.to_string(), bucket);
        let mut count = self.counts.entry(key).or_insert(0);
        *count += 1;
        *count <= self.limit_rpm as u64
    }
}

fn current_minute_bucket() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M").to_string()
}

/// Axum middleware function. Extracts client IP from ConnectInfo and checks
/// the rate limiter. Returns 429 if the limit is exceeded.
///
/// `connect_info` is `Option` so this middleware works in test environments
/// (axum_test::TestServer does not inject ConnectInfo). When None, the request
/// is allowed through unconditionally.
pub async fn ip_rate_limit_middleware(
    State(limiter): State<Arc<IpRateLimiter>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(ConnectInfo(addr)) = connect_info {
        let ip = addr.ip().to_string();
        if !limiter.check_and_increment(&ip) {
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(Body::from("rate limit exceeded"))
                .unwrap();
        }
    }
    next.run(request).await
}
```

- [ ] **Step 4: Create `src/api/middleware/mod.rs`**

```rust
pub mod ip_rate_limit;
```

- [ ] **Step 5: Add `pub mod middleware;` to `src/api/mod.rs`**

Read `src/api/mod.rs` first. Add `pub mod middleware;`.

- [ ] **Step 6: Run unit tests**

```bash
cargo test test_ip_rate_limit 2>&1 | tail -10
```
Expected: all 4 pass

- [ ] **Step 7: Add `ip_rate_limiter` to AppState**

In `src/api/app.rs`, add field:
```rust
pub ip_rate_limiter: Arc<crate::api::middleware::ip_rate_limit::IpRateLimiter>,
```

In `build_router`, after `.with_state(state)`:
```rust
use axum::middleware;
use crate::api::middleware::ip_rate_limit::ip_rate_limit_middleware;

// IP rate limiting middleware — runs before all other processing.
// Applied last so it executes first (axum layers are LIFO).
.layer(middleware::from_fn_with_state(
    state.ip_rate_limiter.clone(),
    ip_rate_limit_middleware,
))
```

- [ ] **Step 8: Wire in `cli/mod.rs`**

Add `ip_rate_limiter` to the `AppState { ... }` construction:
```rust
ip_rate_limiter: Arc::new(crate::api::middleware::ip_rate_limit::IpRateLimiter::new(
    settings.server.ip_rate_limit_rpm,
)),
```

Change the server startup line from:
```rust
axum::serve(listener, app).await?;
```
to:
```rust
use std::net::SocketAddr;
axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
```

- [ ] **Step 9: Update all 8 test files**

Add to each `AppState { ... }` literal:
```rust
ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
```

(Use `0` in tests to disable IP rate limiting by default.)

- [ ] **Step 10: Run full test suite**

```bash
cargo test 2>&1 | tail -15
cargo test --features otel 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 11: Commit**

```bash
git add src/api/middleware/ip_rate_limit.rs src/api/middleware/mod.rs \
        src/api/mod.rs src/api/app.rs src/cli/mod.rs \
        src/config/schema.rs \
        tests/test_ip_rate_limit.rs \
        tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs \
        tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs \
        tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: add in-memory IP rate limiting middleware"
```

---

### Task 3: Per-Tag Budgets

**Files:**
- Create: `migrations/007_api_key_tag.sql`
- Create: `migrations/008_budget_rule_tag.sql`
- Modify: `src/db/models.rs` (`tag` on `ApiKey`; `tag` on `BudgetRule`; `api_key_tag` on `User`)
- Modify: `src/db/sqlite/api_keys.rs` (`ApiKeyRow.tag`, `From<ApiKeyRow>`, all SQL statements)
- Modify: `src/db/sqlite/budgets.rs` (all 5 SELECTs + INSERT)
- Modify: `src/db/postgres/budgets.rs` (same with `$N` placeholders)
- Modify: `src/db/repositories/budgets.rs` (`list_for_tag` to trait)
- Modify: `src/db/sqlite/budgets.rs` (implement `list_for_tag`)
- Modify: `src/api/auth.rs` (set `user.api_key_tag` from looked-up `ApiKey.tag`)
- Modify: `src/router/policy.rs` (include tag-matched rules in `check()`)

**Context:**

Tags work like this: an operator assigns a `tag` string to an API key (e.g., `"ci"` or `"project-x"`). A budget rule can target that same tag. When a user authenticates via a tagged key, the policy engine also checks rules where `rule.tag = key.tag`.

`User` already has `api_key_id: Option<i64>` which is NOT a DB column — it's set in memory by the auth extractor (`user.api_key_id = Some(api_key.id)`). We follow the same pattern for `api_key_tag: Option<String>`: add it to the `User` struct with `#[sqlx(default)]`, and populate it in `auth.rs`.

`src/db/sqlite/budgets.rs` maps directly to `BudgetRule` — there is **no `BudgetRuleRow`** intermediary. `#[sqlx(default)]` on `BudgetRule.tag` is effective and sufficient for NULL-safe deserialization.

`src/db/sqlite/api_keys.rs` DOES use `ApiKeyRow` as the sqlx intermediary. You MUST update `ApiKeyRow`, `From<ApiKeyRow>`, and all 4 SQL statements.

- [ ] **Step 1: Write the failing tests**

Create `tests/test_tag_budgets.rs`:
```rust
// tests/test_tag_budgets.rs
mod common;

use axum_test::TestServer;
use modelrouter::api::app::{build_router, AppState, DatabaseProvider};
use modelrouter::api::auth::hash_token;
use modelrouter::config::Settings;
use modelrouter::db::models::{NewUser, NewApiKey, NewBudgetRule};
use modelrouter::db::repositories::{users::UserRepository, api_keys::ApiKeyRepository, budgets::BudgetRepository};
use modelrouter::providers::registry::ProviderRegistry;
use modelrouter::router::{cost::CostCalculator, engine::RequestRouter, fallback::FallbackChain, policy::PolicyEngine};
use std::collections::HashMap;
use std::sync::Arc;

async fn test_app_with_tag(tag: Option<&str>, budget_tag: Option<&str>) -> (TestServer, i64) {
    let db = common::in_memory_db().await;
    let user = db.create(NewUser {
        name: "tag-user".to_string(),
        api_key_hash: hash_token("tag-token"),
        group_name: None,
    }).await.unwrap();

    let key = db.create_api_key(NewApiKey {
        user_id: user.id,
        key_hash: hash_token("tag-key"),
        label: Some("tagged".to_string()),
        expires_at: None,
        tag: tag.map(str::to_string),
    }).await.unwrap();

    if let Some(t) = budget_tag {
        db.create_budget_rule(NewBudgetRule {
            user_id: None,
            group_name: None,
            api_key_id: None,
            tag: Some(t.to_string()),
            window: "monthly".to_string(),
            limit_usd: Some(0.0001), // tiny limit to trigger denial
            limit_tokens: None,
            model_allow: vec![],
            model_deny: vec![],
            rate_rpm: None,
            max_concurrent: None,
        }).await.unwrap();
    }

    // ... (build AppState with mock adapter, return TestServer and user.id)
    todo!()
}

#[test]
fn api_key_tag_field_compiles() {
    // Minimal test: verify NewApiKey has a tag field
    let _key = modelrouter::db::models::NewApiKey {
        user_id: 1,
        key_hash: "abc".to_string(),
        label: None,
        expires_at: None,
        tag: Some("ci".to_string()),
    };
}

#[test]
fn budget_rule_tag_field_compiles() {
    // Verify NewBudgetRule has a tag field
    let _rule = modelrouter::db::models::NewBudgetRule {
        user_id: None,
        group_name: None,
        api_key_id: None,
        tag: Some("ci".to_string()),
        window: "monthly".to_string(),
        limit_usd: None,
        limit_tokens: None,
        model_allow: vec![],
        model_deny: vec![],
        rate_rpm: None,
        max_concurrent: None,
    };
}

#[test]
fn user_has_api_key_tag_field() {
    // Verify User struct has api_key_tag
    let user = modelrouter::db::models::User {
        id: 1,
        name: "test".to_string(),
        api_key: "hash".to_string(),
        api_key_old: None,
        api_key_old_expires_at: None,
        group_name: None,
        enabled: true,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        metadata: "{}".to_string(),
        api_key_id: None,
        spend_reset_at: None,
        api_key_tag: None,
    };
    assert!(user.api_key_tag.is_none());
}
```

Run: `cargo test test_tag_budgets 2>&1 | head -15`
Expected: compile error — `NewApiKey` has no `tag` field

- [ ] **Step 2: Create migrations**

Create `migrations/007_api_key_tag.sql`:
```sql
-- migrations/007_api_key_tag.sql
ALTER TABLE api_keys ADD COLUMN tag TEXT;
```

Create `migrations/008_budget_rule_tag.sql`:
```sql
-- migrations/008_budget_rule_tag.sql
ALTER TABLE budget_rules ADD COLUMN tag TEXT;
```

- [ ] **Step 3: Update models in `src/db/models.rs`**

Read the file first. Make three changes:

**a) Add `tag` to `ApiKey`:**
```rust
/// Optional tag for per-tag budget matching (e.g., "ci", "project-x").
#[sqlx(default)]
pub tag: Option<String>,
```

**b) Add `tag` to `NewApiKey`:**
```rust
pub tag: Option<String>,
```

**c) Add `tag` to `BudgetRule`:**
```rust
/// If set, this rule applies to API keys with a matching tag.
#[sqlx(default)]
pub tag: Option<String>,
```

**d) Add `tag` to `NewBudgetRule`:**
```rust
pub tag: Option<String>,
```

**e) Add `api_key_tag` to `User` (in-memory only, NOT a DB column):**
```rust
/// Tag from the authenticating API key. Set in memory by auth extractor.
#[sqlx(default)]
pub api_key_tag: Option<String>,
```

- [ ] **Step 4: Update `ApiKeyRow` and SQL in `src/db/sqlite/api_keys.rs`**

Read the file first. `ApiKeyRow` is the sqlx intermediary — update it:

Add to `ApiKeyRow`:
```rust
#[sqlx(default)]
tag: Option<String>,
```

Add to `From<ApiKeyRow>`:
```rust
tag: r.tag,
```

Update all 4 SQL statements to include `tag`:
- `find_api_key_by_hash` SELECT: add `tag`
- `list_api_keys_for_user` SELECT: add `tag`
- `create_api_key` INSERT: add `tag` to column list and `key.tag` to binds
- `create_api_key` SELECT-after-insert: add `tag`

Updated INSERT:
```rust
sqlx::query(
    "INSERT INTO api_keys (user_id, key_hash, label, enabled, created_at, expires_at, tag) \
     VALUES (?, ?, ?, 1, ?, ?, ?)"
)
.bind(key.user_id)
.bind(&key.key_hash)
.bind(&key.label)
.bind(&now)
.bind(&key.expires_at)
.bind(&key.tag)
```

Also check for all `NewApiKey {` struct literals in tests: `grep -rn "NewApiKey {" tests/` — add `tag: None` to each.

- [ ] **Step 5: Update `BudgetRule` SQL in `src/db/sqlite/budgets.rs`**

Read the file. There is NO `BudgetRuleRow` — `BudgetRule` maps directly. Update:
- All 5 SELECT statements: add `tag` to column lists
- The INSERT: add `tag` to column list and bind `rule.tag`
- The `list_for_tag` query (new — see Step 6)

Also update `NewBudgetRule` call sites: `grep -rn "NewBudgetRule {" src/ tests/` — add `tag: None` to each.

- [ ] **Step 6: Add `list_for_tag` to `BudgetRepository` trait and impl**

In `src/db/repositories/budgets.rs`, add to the trait:
```rust
async fn list_for_tag(&self, tag: &str) -> anyhow::Result<Vec<BudgetRule>>;
```

In `src/db/sqlite/budgets.rs`, implement it:
```rust
async fn list_for_tag(&self, tag: &str) -> anyhow::Result<Vec<BudgetRule>> {
    let rows = sqlx::query_as::<_, BudgetRule>(
        "SELECT id, user_id, group_name, api_key_id, tag, window, limit_usd, limit_tokens, \
         model_allow, model_deny, rate_rpm, max_concurrent, created_at, updated_at \
         FROM budget_rules WHERE tag = ?"
    )
    .bind(tag)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows)
}
```

Update `src/db/postgres/budgets.rs` with the same changes: add `tag` to all SELECT/INSERT, add `list_for_tag` impl using `$1`.

- [ ] **Step 7: Populate `api_key_tag` in `src/api/auth.rs`**

Read the file. After `user.api_key_id = Some(api_key.id);`, add:
```rust
user.api_key_tag = api_key.tag.clone();
```

- [ ] **Step 8: Include tag rules in `src/router/policy.rs`**

Read the file. In the `check()` method, find where per-key rules are loaded. After loading per-key rules, add:

```rust
// Include budget rules targeting this key's tag.
// Tag rules have lowest priority — append AFTER user/group/key rules so that
// more-specific rules evaluated earlier can override them. For model access
// lists the first matching Allow/Deny wins, so appending gives tag rules
// lowest precedence.
if let Some(tag) = &user.api_key_tag {
    let tag_rules = self.db.list_for_tag(tag).await
        .unwrap_or_default();
    rules.extend(tag_rules);
}
```

(Position: after all other rule loading, so tag rules are appended at the end with lowest priority.)

- [ ] **Step 9: Run tests**

```bash
cargo test test_tag_budgets 2>&1 | tail -10
cargo test 2>&1 | tail -15
cargo test --features otel 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 10: Commit**

```bash
git add migrations/007_api_key_tag.sql migrations/008_budget_rule_tag.sql \
        src/db/models.rs src/db/sqlite/api_keys.rs \
        src/db/sqlite/budgets.rs src/db/postgres/budgets.rs \
        src/db/repositories/budgets.rs \
        src/api/auth.rs src/router/policy.rs \
        tests/test_tag_budgets.rs
git commit -m "feat: add per-tag budgets (tag on api_keys, tag-matched budget rules)"
```

---

## Common Pitfalls

1. **`ApiKeyRow` intermediary** — `ApiKey` uses `ApiKeyRow` for sqlx deserialization in `src/db/sqlite/api_keys.rs`. `#[sqlx(default)]` on `ApiKey` is inert. Always update `ApiKeyRow` AND `From<ApiKeyRow>` AND all SQL statements.

2. **No `BudgetRuleRow`** — `src/db/sqlite/budgets.rs` maps directly to `BudgetRule` (confirmed: no BudgetRuleRow struct exists). `#[sqlx(default)]` on `BudgetRule` is effective here.

3. **`api_key_tag` is in-memory only** — `User.api_key_tag` follows the same pattern as `User.api_key_id`: it is set by the auth extractor after key lookup, not persisted in the DB. Do NOT add it to `UserRow` or any SELECT statement.

4. **Axum LIFO layer order** — middleware applied via `.layer()` executes in reverse order. The IP rate limit middleware must be the LAST `.layer()` call in `build_router` to ensure it runs FIRST (before auth).

5. **`into_make_service_with_connect_info`** — Required for `ConnectInfo<SocketAddr>` to be available in middleware. The server startup in `cli/mod.rs` must use this instead of plain `into_make_service()`.

6. **8 test files + AppState fields** — Task 1 adds `circuit_breaker`; Task 2 adds `ip_rate_limiter`. Both need to be added to all 8 AppState test files: `test_completions.rs`, `test_cache.rs`, `test_embeddings.rs`, `test_messages.rs`, `test_per_key_budgets.rs`, `test_dashboard.rs`, `test_prometheus.rs`, `test_telemetry.rs`.

7. **`test_telemetry.rs` is gated `#[cfg(feature = "otel")]`** — plain `cargo test` passes even if this file has compile errors. Always run `cargo test --features otel`.

8. **Postgres parity** — `src/db/postgres/budgets.rs` needs the same changes as `src/db/sqlite/budgets.rs` but with `$N`-style placeholders instead of `?`.

9. **`test_policy.rs` has `NewBudgetRule` literals** — it is NOT in the "8 AppState test files" list but it does contain `NewBudgetRule { ... }` struct literals that will fail to compile when `tag` is added to `NewBudgetRule`. Run `grep -rn "NewBudgetRule {" tests/` to find all sites and add `tag: None`.
