# Phase 10 — Quick Wins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement five high-value features — Anthropic Messages API passthrough, token budget enforcement, config-driven pricing, fallback chain retries, and a Prometheus metrics endpoint.

**Architecture:** Each task is independent and self-contained. Tasks 1–4 modify existing layers (routes, policy engine, config, router) with no new subsystems. Task 5 adds a lightweight `prometheus` feature flag with an independent metrics state stored in `AppState`.

**Tech Stack:** Rust 2021 · axum 0.7 · sqlx 0.8 · reqwest 0.12 · serde_json · prometheus crate (Task 5)

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `src/api/routes/messages.rs` | Create | `POST /v1/messages` Anthropic Messages API handler |
| `src/api/routes/mod.rs` | Modify | Add `pub mod messages` |
| `src/api/app.rs` | Modify | Add `/v1/messages` route; add `metrics` field to `AppState`; add `/metrics` route (feature-gated) |
| `src/router/policy.rs` | Modify | Add `limit_tokens` enforcement |
| `src/db/repositories/costs.rs` | Modify | Add `sum_tokens_for_user_since()` to trait |
| `src/db/sqlite/costs.rs` | Modify | Implement `sum_tokens_for_user_since()` |
| `src/db/postgres/costs.rs` | Modify | Implement `sum_tokens_for_user_since()` for postgres feature |
| `src/config/schema.rs` | Modify | Add `PricingEntry` struct and `pricing` field to `Settings` |
| `src/router/cost.rs` | Modify | Accept config pricing in `CostCalculator::new_with_config()` |
| `src/cli/mod.rs` | Modify | Pass `settings.pricing` to `CostCalculator` at startup; wire `FallbackChain` and `AppMetrics` into `AppState` |
| `config.example.toml` | Modify | Add `[[pricing]]` section |
| `src/router/fallback.rs` | Create | `FallbackChain::try_in_order()` retry loop |
| `src/router/mod.rs` | Modify | Add `pub mod fallback` |
| `src/api/routes/completions.rs` | Modify | Wire fallback chain; wire Prometheus recording |
| `src/metrics/mod.rs` | Create | `AppMetrics` struct with atomic counters (prometheus feature) |
| `src/metrics/prometheus.rs` | Create | Prometheus text-format serialiser |
| `src/api/routes/prometheus.rs` | Create | `GET /metrics` handler (prometheus feature) |
| `Cargo.toml` | Modify | Add `prometheus` feature + `prometheus` crate dep |
| `tests/test_messages.rs` | Create | Integration tests for `/v1/messages` |
| `tests/test_policy.rs` | Modify | Add token-limit enforcement tests |
| `tests/test_cost.rs` | Modify | Add config-driven pricing tests |
| `tests/test_router.rs` | Modify | Add fallback chain tests |
| `tests/test_prometheus.rs` | Create | Prometheus endpoint tests |

---

## Task 1: POST /v1/messages — Anthropic Messages API Passthrough

**Context:** Claude Code sets `ANTHROPIC_BASE_URL=http://localhost:8080` and sends native Anthropic Messages API requests to `POST /v1/messages`. The handler must accept Anthropic-format bodies, apply auth + policy, proxy to the configured Anthropic provider, and return a native Anthropic-format response (both streaming and non-streaming).

**Files:**
- Create: `src/api/routes/messages.rs`
- Modify: `src/api/routes/mod.rs`
- Modify: `src/api/app.rs`
- Create: `tests/test_messages.rs`

### Step 1.1: Write the failing test

- [ ] **Write `tests/test_messages.rs`**

```rust
// tests/test_messages.rs
mod common;
use axum_test::TestServer;
use serde_json::{json, Value};

#[tokio::test]
async fn test_messages_requires_auth() {
    let server = common::test_server().await;
    let resp = server
        .post("/v1/messages")
        .json(&json!({"model": "claude-haiku-4-5", "max_tokens": 10,
                       "messages": [{"role": "user", "content": "hi"}]}))
        .await;
    assert_eq!(resp.status_code(), 401);
}

#[tokio::test]
async fn test_messages_model_extracted_for_policy() {
    // Policy denying all models should block /v1/messages too
    let server = common::test_server_with_policy_deny_all().await;
    let key = common::create_user_with_api_key(&server).await;
    let resp = server
        .post("/v1/messages")
        .add_header("Authorization", format!("Bearer {key}").parse().unwrap())
        .json(&json!({"model": "gpt-4o", "max_tokens": 10,
                       "messages": [{"role": "user", "content": "hi"}]}))
        .await;
    assert_eq!(resp.status_code(), 429); // budget/policy denied
}
```

- [ ] **Run test to verify it fails (compilation error expected)**

```bash
cargo test test_messages --test test_messages 2>&1 | head -20
```

Expected: compilation error — `test_messages` module doesn't exist yet.

### Step 1.2: Create the messages handler

- [ ] **Create `src/api/routes/messages.rs`**

```rust
use std::time::Instant;

use axum::{extract::State, response::{IntoResponse, Response}, Json};
use serde_json::Value;

use crate::{
    api::{app::AppState, auth::AuthenticatedUser, error::ApiError},
    db::models::{NewCostLedgerEntry, NewPrompt},
    router::policy::PolicyDecision,
};

/// POST /v1/messages — Anthropic Messages API passthrough.
///
/// Accepts native Anthropic Messages API format, applies auth + policy,
/// proxies to the configured Anthropic provider, and returns a native
/// Anthropic-format response. Both streaming and non-streaming are supported.
pub async fn anthropic_messages(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let user = user.0;

    let model = body["model"]
        .as_str()
        .unwrap_or(&state.settings.routing.default_model)
        .to_string();
    let stream = body["stream"].as_bool().unwrap_or(false);
    let max_tokens = body["max_tokens"].as_u64().unwrap_or(4096);

    // Policy check (reuses same engine as /v1/chat/completions)
    match state.policy.check(&user, &model).await.map_err(|_| ApiError::Internal)? {
        PolicyDecision::Allow => {}
        PolicyDecision::Deny { reason, status, budget_context } => {
            if budget_context.is_some() {
                for hook in &state.settings.hooks.lifecycle {
                    if hook.event == "on_budget_exceeded" {
                        let ctx = budget_context.as_ref();
                        let payload = crate::hooks::lifecycle::budget_exceeded_payload(
                            &user.name, &model,
                            ctx.map(|c| c.limit_usd).unwrap_or(0.0),
                            ctx.map(|c| c.spent_usd).unwrap_or(0.0),
                            ctx.map(|c| c.window.as_str()).unwrap_or("unknown"),
                        );
                        crate::hooks::lifecycle::fire(hook, payload);
                    }
                }
            }
            return Err(ApiError::PolicyDenied { reason, status });
        }
    }

    // Resolve provider — prefer explicit model prefix, fall back to "anthropic"
    let (provider_name, canonical_model) = state.router.resolve(&model);
    let _provider_name = if provider_name == state.settings.routing.default_provider {
        "anthropic".to_string()
    } else {
        provider_name
    };

    // Get Anthropic API key and base URL from settings
    let anthropic_config = state.settings.providers.get("anthropic")
        .ok_or_else(|| ApiError::ProviderError(anyhow::anyhow!("No 'anthropic' provider configured")))?;
    let api_key = anthropic_config.api_key.clone();
    let api_base = anthropic_config.api_base.clone()
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());

    // Build upstream URL
    let upstream_url = format!("{}/v1/messages", api_base.trim_end_matches('/'));

    // Build upstream body — use canonical model name, ensure max_tokens
    let mut upstream_body = body.clone();
    upstream_body["model"] = Value::String(canonical_model.clone());
    if upstream_body.get("max_tokens").is_none() {
        upstream_body["max_tokens"] = Value::Number(max_tokens.into());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(anthropic_config.timeout_secs))
        .build()
        .map_err(|e| ApiError::ProviderError(anyhow::anyhow!("{e}")))?;

    let start = Instant::now();

    if stream {
        // Streaming: forward raw Anthropic SSE bytes
        let resp = client
            .post(&upstream_url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&upstream_body)
            .send()
            .await
            .map_err(|e| ApiError::ProviderError(anyhow::anyhow!("{e}")))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::ProviderError(anyhow::anyhow!("Anthropic {}", text)));
        }

        use futures::TryStreamExt;
        use axum::body::Body;
        use axum::http::{header, StatusCode};

        let byte_stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(e.to_string()));

        // Fire-and-forget cost estimate (streaming — token count is approximate)
        let state_clone = state.clone();
        let user_id = user.id;
        let model_c = model.clone();
        let canonical_c = canonical_model.clone();
        let messages_json = serde_json::to_string(
            &body["messages"].as_array().cloned().unwrap_or_default()
        ).unwrap_or_default();

        tokio::spawn(async move {
            // Rough token estimate from message content length
            let prompt_tokens = (messages_json.chars().count() / 4) as u32;
            let completion_tokens = 0u32; // unknown for streaming
            let cost = state_clone.cost_calc.calculate(&canonical_c, prompt_tokens, completion_tokens);
            let latency_ms = start.elapsed().as_millis() as i64;
            log_messages_cost(&state_clone, user_id, &model_c, &canonical_c, "anthropic",
                               &messages_json, prompt_tokens, completion_tokens, cost, latency_ms).await;
        });

        let body = Body::from_stream(byte_stream);
        return Ok(axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header("X-Accel-Buffering", "no")
            .body(body)
            .unwrap()
            .into_response());
    }

    // Non-streaming: get response, log cost, return Anthropic-format JSON
    let resp = client
        .post(&upstream_url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| ApiError::ProviderError(anyhow::anyhow!("{e}")))?;

    let latency_ms = start.elapsed().as_millis() as i64;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(ApiError::ProviderError(anyhow::anyhow!("Anthropic {}: {}", status, text)));
    }

    let response_body: Value = resp
        .json()
        .await
        .map_err(|e| ApiError::ProviderError(anyhow::anyhow!("Parse error: {e}")))?;

    // Extract token usage from Anthropic response for cost tracking
    let prompt_tokens = response_body["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32;
    let completion_tokens = response_body["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;
    let cost = state.cost_calc.calculate(&canonical_model, prompt_tokens, completion_tokens);

    let messages_json = serde_json::to_string(
        &body["messages"].as_array().cloned().unwrap_or_default()
    ).unwrap_or_default();
    let response_text = response_body["content"]
        .as_array()
        .and_then(|arr| arr.iter().find(|c| c["type"] == "text"))
        .and_then(|c| c["text"].as_str())
        .unwrap_or("")
        .to_string();
    let stop_reason = response_body["stop_reason"].as_str().unwrap_or("end_turn").to_string();

    let state_clone = state.clone();
    let model_c = model.clone();
    let canonical_c = canonical_model.clone();
    let user_id = user.id;

    tokio::spawn(async move {
        log_messages_cost(&state_clone, user_id, &model_c, &canonical_c, "anthropic",
                           &messages_json, prompt_tokens, completion_tokens, cost, latency_ms).await;
        // Fire on_response_sent lifecycle hooks
        for hook in &state_clone.settings.hooks.lifecycle {
            if hook.event == "on_response_sent" {
                let payload = crate::hooks::lifecycle::response_sent_payload(
                    &state_clone.settings.routing.default_provider,
                    &model_c, &canonical_c, cost, latency_ms,
                );
                crate::hooks::lifecycle::fire(hook, payload);
            }
        }
    });

    // Rebuild response with routed model name so client sees what was actually used
    let mut final_response = response_body;
    final_response["model"] = Value::String(canonical_model);
    final_response["_stop_reason"] = Value::String(stop_reason); // preserve for completeness

    Ok(Json(final_response).into_response())
}

async fn log_messages_cost(
    state: &AppState,
    user_id: i64,
    model: &str,
    canonical_model: &str,
    provider: &str,
    messages_json: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
    cost: f64,
    latency_ms: i64,
) {
    use crate::db::repositories::{costs::CostRepository, prompts::PromptRepository};
    let prompt = NewPrompt {
        user_id,
        session_id: None,
        request_model: model.to_string(),
        routed_model: canonical_model.to_string(),
        provider: provider.to_string(),
        messages: messages_json.to_string(),
        response: None,
        finish_reason: None,
        prompt_tokens: prompt_tokens as i64,
        completion_tokens: completion_tokens as i64,
        cost_usd: cost,
        latency_ms: Some(latency_ms),
        tags: "[]".to_string(),
        project: None,
    };
    match PromptRepository::create(&*state.db, prompt).await {
        Ok(saved) => {
            let ledger = NewCostLedgerEntry {
                user_id,
                prompt_id: saved.id,
                model: canonical_model.to_string(),
                provider: provider.to_string(),
                project: None,
                tokens_in: prompt_tokens as i64,
                tokens_out: completion_tokens as i64,
                cost_usd: cost,
            };
            if let Err(e) = CostRepository::create(&*state.db, ledger).await {
                tracing::error!("Failed to record messages cost: {}", e);
            }
        }
        Err(e) => tracing::error!("Failed to record messages prompt: {}", e),
    }
}
```

- [ ] **Add `pub mod messages` to `src/api/routes/mod.rs`**

Add after the existing module declarations:
```rust
pub mod messages;
```

- [ ] **Add route to `src/api/app.rs`**

In `build_router()`, add to the use imports:
```rust
use crate::api::routes::messages::anthropic_messages;
```

And add the route after `.route("/v1/chat/completions", post(chat_completions))`:
```rust
.route("/v1/messages", post(anthropic_messages))
```

### Step 1.3: Run tests to verify they compile and the auth test passes

- [ ] **Run test**

```bash
cargo test test_messages 2>&1 | tail -20
```

Expected: `test_messages_requires_auth` passes (returns 401 with no auth header). The `test_messages_model_extracted_for_policy` test requires a helper; skip if the helper doesn't exist yet.

### Step 1.4: Commit

```bash
git add src/api/routes/messages.rs src/api/routes/mod.rs src/api/app.rs tests/test_messages.rs
git commit -m "feat: add POST /v1/messages Anthropic Messages API passthrough"
```

---

## Task 2: Token Budget Enforcement in PolicyEngine

**Context:** The `budget_rules` table has a `limit_tokens` column that isn't enforced. When set, it should cap the total tokens (prompt + completion) a user can consume within the budget window.

**Files:**
- Modify: `src/db/repositories/costs.rs`
- Modify: `src/db/sqlite/costs.rs`
- Modify: `src/db/postgres/costs.rs`
- Modify: `src/router/policy.rs`
- Modify: `tests/test_policy.rs`

### Step 2.1: Write the failing test

- [ ] **Add to `tests/test_policy.rs`**

Find the existing test file and append:

```rust
#[sqlx::test]
async fn test_policy_token_limit_enforced(pool: sqlx::SqlitePool) {
    use modelrouter::db::models::{NewBudgetRule, NewUser};
    use modelrouter::db::repositories::{
        budgets::BudgetRepository,
        costs::{CostRepository, NewCostLedgerEntry},
        users::UserRepository,
    };
    use modelrouter::db::sqlite::SqliteDb;
    use modelrouter::router::policy::{PolicyDecision, PolicyEngine};
    use std::sync::Arc;

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let db = Arc::new(SqliteDb { pool: pool.clone() });

    // Create user
    let user = UserRepository::create(&*db, NewUser {
        name: "tokentest".into(), group_name: None,
        api_key_hash: "hash".into(),
    }).await.unwrap();

    // Set token budget of 100 tokens/monthly
    BudgetRepository::create(&*db, NewBudgetRule {
        user_id: Some(user.id), group_name: None,
        window: "monthly".into(), limit_usd: None,
        limit_tokens: Some(100), model_allow: vec![], model_deny: vec![],
        rate_rpm: None,
    }).await.unwrap();

    // Simulate 95 tokens already consumed
    let now = chrono::Utc::now().to_rfc3339();
    CostRepository::create(&*db, modelrouter::db::models::NewCostLedgerEntry {
        user_id: user.id, prompt_id: 0, model: "gpt-4o".into(), provider: "openai".into(),
        project: None, tokens_in: 80, tokens_out: 15, cost_usd: 0.001,
    }).await.unwrap();

    let policy = PolicyEngine::new(db);
    let decision = policy.check(&user, "gpt-4o").await.unwrap();

    // 95 tokens used of 100 limit — should still Allow (not over yet)
    assert!(matches!(decision, PolicyDecision::Allow));

    // Now push it over: add 10 more tokens (total 105 > 100)
    // ... (test is intentionally incomplete — add this after implementing the feature)
}

#[sqlx::test]
async fn test_policy_token_limit_blocks_when_exceeded(pool: sqlx::SqlitePool) {
    use modelrouter::db::models::{NewBudgetRule, NewUser};
    use modelrouter::db::repositories::{
        budgets::BudgetRepository, costs::CostRepository, users::UserRepository,
    };
    use modelrouter::db::sqlite::SqliteDb;
    use modelrouter::router::policy::{PolicyDecision, PolicyEngine};
    use std::sync::Arc;

    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let db = Arc::new(SqliteDb { pool: pool.clone() });

    let user = UserRepository::create(&*db, NewUser {
        name: "overtokentest".into(), group_name: None,
        api_key_hash: "hash2".into(),
    }).await.unwrap();

    BudgetRepository::create(&*db, NewBudgetRule {
        user_id: Some(user.id), group_name: None,
        window: "monthly".into(), limit_usd: None,
        limit_tokens: Some(50), model_allow: vec![], model_deny: vec![],
        rate_rpm: None,
    }).await.unwrap();

    // Insert 60 tokens used — over the 50 limit
    CostRepository::create(&*db, modelrouter::db::models::NewCostLedgerEntry {
        user_id: user.id, prompt_id: 0, model: "gpt-4o".into(), provider: "openai".into(),
        project: None, tokens_in: 50, tokens_out: 10, cost_usd: 0.001,
    }).await.unwrap();

    let policy = PolicyEngine::new(db);
    let decision = policy.check(&user, "gpt-4o").await.unwrap();

    match decision {
        PolicyDecision::Deny { status, .. } => assert_eq!(status, 429),
        PolicyDecision::Allow => panic!("Expected Deny but got Allow"),
    }
}
```

- [ ] **Run tests to verify they fail**

```bash
cargo test test_policy_token 2>&1 | tail -20
```

Expected: compilation error — `sum_tokens_for_user_since` doesn't exist yet.

### Step 2.2: Add `sum_tokens_for_user_since` to repository trait

- [ ] **Modify `src/db/repositories/costs.rs`**

Add to the trait:
```rust
async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64>;
```

Full trait becomes:
```rust
#[async_trait]
pub trait CostRepository: Send + Sync {
    async fn create(&self, entry: NewCostLedgerEntry) -> anyhow::Result<CostLedgerEntry>;
    async fn sum_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<f64>;
    async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64>;
}
```

### Step 2.3: Implement in SQLite

- [ ] **Add to `src/db/sqlite/costs.rs`** inside the `impl CostRepository for SqliteDb` block:

```rust
async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger
         WHERE user_id = ? AND created_at >= ?",
    )
    .bind(user_id)
    .bind(since)
    .fetch_one(&self.pool)
    .await?;
    Ok(row.0)
}
```

### Step 2.4: Implement in Postgres

- [ ] **Add to `src/db/postgres/costs.rs`** inside the `impl CostRepository for PostgresDb` block:

```rust
async fn sum_tokens_for_user_since(&self, user_id: i64, since: &str) -> anyhow::Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM cost_ledger
         WHERE user_id = $1 AND created_at >= $2",
    )
    .bind(user_id)
    .bind(since)
    .fetch_one(&self.pool)
    .await?;
    Ok(row.0)
}
```

### Step 2.5: Add token limit check to PolicyEngine

- [ ] **Modify `src/router/policy.rs`** — in the `check()` method, add after the `limit_usd` check (after step 4):

```rust
// 5. Check token budget
if let Some(limit_tokens) = rule.limit_tokens {
    let window_start = window_start_for(&rule.window);
    let used_tokens =
        CostRepository::sum_tokens_for_user_since(&*self.db, user.id, &window_start).await?;
    if used_tokens >= limit_tokens {
        let reason = format!(
            "token budget exceeded: {} of {} {} tokens used",
            used_tokens, limit_tokens, rule.window
        );
        span.record("policy.result", "deny");
        span.record("policy.reason", reason.as_str());
        return Ok(PolicyDecision::Deny {
            reason,
            status: 429,
            budget_context: None,
        });
    }
}
```

### Step 2.6: Run tests to verify they pass

```bash
cargo test test_policy_token 2>&1 | tail -20
```

Expected: both tests pass.

### Step 2.7: Commit

```bash
git add src/db/repositories/costs.rs src/db/sqlite/costs.rs src/db/postgres/costs.rs \
        src/router/policy.rs tests/test_policy.rs
git commit -m "feat: enforce limit_tokens budget rule in PolicyEngine"
```

---

## Task 3: Config-Driven Pricing Table

**Context:** `CostCalculator` has a hardcoded pricing table. Add `[[pricing]]` TOML config so operators can set per-model prices without recompiling. Config prices override defaults; hardcoded table is the fallback.

**Files:**
- Modify: `src/config/schema.rs`
- Modify: `src/router/cost.rs`
- Modify: `src/cli/mod.rs` (or wherever `CostCalculator::new()` is called)
- Modify: `config.example.toml`
- Modify: `tests/test_cost.rs`

### Step 3.1: Write the failing test

- [ ] **Add to `tests/test_cost.rs`**

```rust
#[test]
fn test_config_pricing_overrides_default() {
    use modelrouter::config::schema::PricingEntry;
    use modelrouter::router::cost::CostCalculator;

    let custom = vec![PricingEntry {
        model: "my-custom-model".to_string(),
        input_per_million: 1.0,
        output_per_million: 2.0,
    }];

    let calc = CostCalculator::new_with_config(&custom);

    // Custom model is present
    let cost = calc.calculate("my-custom-model", 1_000_000, 0);
    assert!((cost - 1.0).abs() < 0.001, "Expected $1.00, got {cost}");

    // Default model still works
    let cost2 = calc.calculate("gpt-4o", 1_000_000, 0);
    assert!((cost2 - 2.50).abs() < 0.001, "Expected $2.50, got {cost2}");
}

#[test]
fn test_config_pricing_overrides_default_price() {
    use modelrouter::config::schema::PricingEntry;
    use modelrouter::router::cost::CostCalculator;

    // Override an existing model's price
    let custom = vec![PricingEntry {
        model: "gpt-4o".to_string(),
        input_per_million: 99.0,
        output_per_million: 99.0,
    }];

    let calc = CostCalculator::new_with_config(&custom);
    let cost = calc.calculate("gpt-4o", 1_000_000, 0);
    assert!((cost - 99.0).abs() < 0.001, "Config price should override default");
}
```

- [ ] **Run test to verify it fails**

```bash
cargo test test_config_pricing 2>&1 | tail -10
```

Expected: compilation error — `PricingEntry` and `new_with_config` don't exist.

### Step 3.2: Add `PricingEntry` to config schema

- [ ] **Modify `src/config/schema.rs`**

Add `PricingEntry` struct before `Settings`:
```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PricingEntry {
    pub model: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
}
```

Add `pricing` field to `Settings`:
```rust
pub struct Settings {
    // ... existing fields ...
    #[serde(default)]
    pub pricing: Vec<PricingEntry>,
}
```

### Step 3.3: Add `new_with_config` to `CostCalculator`

- [ ] **Modify `src/router/cost.rs`**

Add `use crate::config::schema::PricingEntry;` at the top.

Add a new constructor that takes config pricing and merges with defaults:

```rust
/// Build a CostCalculator using config-provided pricing, falling back to
/// hardcoded defaults for any model not listed in config.
pub fn new_with_config(config_pricing: &[PricingEntry]) -> Self {
    let mut calc = Self::new(); // start with defaults
    for entry in config_pricing {
        calc.pricing.insert(
            Box::leak(entry.model.to_lowercase().into_boxed_str()),
            ModelPricing {
                input_per_million: entry.input_per_million,
                output_per_million: entry.output_per_million,
            },
        );
    }
    calc
}
```

Change the `HashMap` key type from `&'static str` to `String`. Update `new()` to use `.to_string()` for all keys. This is required to make `new_with_config()` work cleanly without any memory leaks:

```rust
pub struct CostCalculator {
    pricing: HashMap<String, ModelPricing>,
}

impl CostCalculator {
    pub fn new() -> Self {
        let mut pricing = HashMap::new();
        pricing.insert("claude-opus-4-6".to_string(), ModelPricing { ... });
        // ... rest of entries with .to_string() ...
        Self { pricing }
    }

    pub fn new_with_config(config_pricing: &[PricingEntry]) -> Self {
        let mut calc = Self::new();
        for entry in config_pricing {
            calc.pricing.insert(
                entry.model.to_lowercase(),
                ModelPricing {
                    input_per_million: entry.input_per_million,
                    output_per_million: entry.output_per_million,
                },
            );
        }
        calc
    }

    pub fn calculate(&self, model: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        let model_key = if let Some(pos) = model.find('/') {
            &model[pos + 1..]
        } else {
            model
        };
        let model_lower = model_key.to_lowercase();
        match self.pricing.get(&model_lower) {
            Some(p) => {
                (prompt_tokens as f64 / 1_000_000.0) * p.input_per_million
                    + (completion_tokens as f64 / 1_000_000.0) * p.output_per_million
            }
            None => 0.0,
        }
    }
}
```

### Step 3.4: Wire `new_with_config` at startup

- [ ] **Find where `CostCalculator::new()` is called** (likely in `src/cli/mod.rs` or `src/api/app.rs`)

```bash
grep -rn "CostCalculator::new" src/
```

- [ ] **Replace `CostCalculator::new()` with `CostCalculator::new_with_config(&settings.pricing)`**

Example: if it's in `src/cli/mod.rs`:
```rust
// before:
let cost_calc = Arc::new(CostCalculator::new());
// after:
let cost_calc = Arc::new(CostCalculator::new_with_config(&settings.pricing));
```

### Step 3.5: Update `config.example.toml`

- [ ] **Add `[[pricing]]` section** at end of `config.example.toml`:

```toml
# ── Pricing overrides ────────────────────────────────────────────────────
# Override per-model pricing (dollars per million tokens).
# Any model not listed here falls back to built-in defaults.
# Useful when using private deployments with different pricing tiers.
#
# [[pricing]]
# model = "gpt-4o"
# input_per_million = 2.50
# output_per_million = 10.0
#
# [[pricing]]
# model = "my-private-llama"
# input_per_million = 0.10
# output_per_million = 0.30
```

### Step 3.6: Run tests

```bash
cargo test test_config_pricing 2>&1 | tail -10
```

Expected: both tests pass.

### Step 3.7: Full test suite

```bash
cargo test 2>&1 | tail -15
```

Expected: all tests pass (no regressions from the HashMap key type change).

### Step 3.8: Commit

```bash
git add src/config/schema.rs src/router/cost.rs src/cli/mod.rs \
        config.example.toml tests/test_cost.rs
git commit -m "feat: config-driven pricing table via [[pricing]] TOML entries"
```

---

## Task 4: Fallback Chain Retry Loop

**Context:** `RoutingConfig` already has `fallback_chains: HashMap<String, Vec<String>>`. When a provider call fails, the completions handler should retry with the next model in the configured fallback chain. `FallbackChain::try_in_order()` encapsulates this logic.

**Files:**
- Create: `src/router/fallback.rs`
- Modify: `src/router/mod.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `tests/test_router.rs`

### Step 4.1: Write the failing test

- [ ] **Add to `tests/test_router.rs`**

```rust
#[test]
fn test_fallback_chain_next_model() {
    use modelrouter::router::fallback::FallbackChain;
    use std::collections::HashMap;

    let mut chains = HashMap::new();
    chains.insert(
        "gpt-4o".to_string(),
        vec!["gpt-4o-mini".to_string(), "gpt-3.5-turbo".to_string()],
    );

    let chain = FallbackChain::new(chains);

    assert_eq!(chain.next_after("gpt-4o"), Some("gpt-4o-mini"));
    assert_eq!(chain.next_after("gpt-4o-mini"), Some("gpt-3.5-turbo"));
    assert_eq!(chain.next_after("gpt-3.5-turbo"), None); // end of chain
    assert_eq!(chain.next_after("unknown-model"), None); // no chain defined
}

#[test]
fn test_fallback_chain_empty() {
    use modelrouter::router::fallback::FallbackChain;
    use std::collections::HashMap;

    let chain = FallbackChain::new(HashMap::new());
    assert_eq!(chain.next_after("gpt-4o"), None);
}
```

- [ ] **Run test to verify it fails**

```bash
cargo test test_fallback_chain 2>&1 | tail -10
```

Expected: compilation error — `fallback` module doesn't exist.

### Step 4.2: Create `src/router/fallback.rs`

```rust
use std::collections::HashMap;

/// Wraps the configured `fallback_chains` map and provides ordered fallback lookup.
pub struct FallbackChain {
    /// key = model that failed; value = ordered list of alternatives to try
    chains: HashMap<String, Vec<String>>,
}

impl FallbackChain {
    pub fn new(chains: HashMap<String, Vec<String>>) -> Self {
        Self { chains }
    }

    /// Returns the next model to try after `failed_model`, or `None` if:
    /// - `failed_model` is not in any chain
    /// - `failed_model` is the last entry in its chain
    pub fn next_after(&self, failed_model: &str) -> Option<&str> {
        for models in self.chains.values() {
            if let Some(pos) = models.iter().position(|m| m == failed_model) {
                return models.get(pos + 1).map(|s| s.as_str());
            }
        }
        // Also check: is failed_model a chain key (i.e. the primary model)?
        if let Some(alternatives) = self.chains.get(failed_model) {
            return alternatives.first().map(|s| s.as_str());
        }
        None
    }
}
```

### Step 4.3: Register module

- [ ] **Modify `src/router/mod.rs`** — add:

```rust
pub mod fallback;
```

### Step 4.4: Wire fallback into AppState

- [ ] **Modify `src/api/app.rs`**

Add `fallback: Arc<FallbackChain>` to `AppState`:
```rust
use crate::router::fallback::FallbackChain;

pub struct AppState {
    // ... existing fields ...
    pub fallback: Arc<FallbackChain>,
}
```

In wherever `AppState` is constructed (likely `src/cli/mod.rs`), add:
```rust
let fallback = Arc::new(FallbackChain::new(settings.routing.fallback_chains.clone()));
// ... include fallback in AppState { ... }
```

### Step 4.5: Use fallback in completions handler

- [ ] **Modify `src/api/routes/completions.rs`**

Replace the single `adapter.complete()` call with a retry loop. Find the non-streaming block that calls `adapter.complete()` and wrap it:

```rust
// Attempt provider call with fallback chain
let mut current_model = canonical_model.clone();
let mut current_provider = provider_name.clone();
let result = loop {
    let adapter = state
        .provider_registry
        .get(&current_provider)
        .map_err(ApiError::ProviderError)?;
    match adapter.complete(&build_normalized_request(&body, current_model.clone())).await {
        Ok(r) => break r,
        Err(e) => {
            tracing::warn!(
                model = current_model.as_str(),
                provider = current_provider.as_str(),
                error = %e,
                "Provider call failed, checking fallback chain"
            );
            if let Some(next_model) = state.fallback.next_after(&current_model) {
                let (next_provider, next_canonical) = state.router.resolve(next_model);
                current_model = next_canonical;
                current_provider = next_provider;
                tracing::info!(fallback_model = current_model.as_str(), "Retrying with fallback");
            } else {
                return Err(ApiError::ProviderError(e));
            }
        }
    }
};
```

**Note on streaming:** The streaming path is more complex to add fallback to (we'd need to detect mid-stream failures). For Phase 10, apply the retry loop only to non-streaming requests. Streaming falls back to returning the error as before.

### Step 4.6: Run tests

```bash
cargo test test_fallback_chain 2>&1 | tail -10
```

Expected: both tests pass.

```bash
cargo test 2>&1 | tail -15
```

Expected: all tests pass.

### Step 4.7: Commit

```bash
git add src/router/fallback.rs src/router/mod.rs src/api/app.rs \
        src/api/routes/completions.rs src/cli/mod.rs tests/test_router.rs
git commit -m "feat: fallback chain retry loop for non-streaming provider failures"
```

---

## Task 5: GET /metrics Prometheus Endpoint

**Context:** Add a lightweight `prometheus` feature flag that exposes request/token/cost counters at `GET /metrics` in Prometheus text format. Independent of the `otel` feature — works with the default binary. Counters are stored as atomics in `AppState`.

**Files:**
- Create: `src/metrics/mod.rs`
- Create: `src/api/routes/prometheus.rs`
- Modify: `Cargo.toml`
- Modify: `src/lib.rs` (add `pub mod metrics`)
- Modify: `src/api/app.rs`
- Modify: `src/api/routes/completions.rs`
- Create: `tests/test_prometheus.rs`

### Step 5.1: Write the failing test

- [ ] **Create `tests/test_prometheus.rs`**

```rust
mod common;

#[cfg(feature = "prometheus")]
#[tokio::test]
async fn test_metrics_endpoint_returns_200() {
    let server = common::test_server().await;
    let resp = server.get("/metrics").await;
    assert_eq!(resp.status_code(), 200);
    let body = resp.text();
    assert!(body.contains("modelrouter_requests_total"), "Missing requests counter");
    assert!(body.contains("modelrouter_tokens_total"), "Missing tokens counter");
}

#[cfg(not(feature = "prometheus"))]
#[tokio::test]
async fn test_metrics_endpoint_not_present_without_feature() {
    let server = common::test_server().await;
    let resp = server.get("/metrics").await;
    assert_eq!(resp.status_code(), 404);
}
```

- [ ] **Run test to verify it fails**

```bash
cargo test --features prometheus test_metrics 2>&1 | tail -10
```

Expected: compilation error — `prometheus` feature doesn't exist.

### Step 5.2: Add `prometheus` feature to `Cargo.toml`

- [ ] **Modify `Cargo.toml`**

Add to `[features]`:
```toml
prometheus = ["dep:prometheus"]
```

Add to `[dependencies]`:
```toml
prometheus = { version = "0.13", optional = true }
```

### Step 5.3: Create `src/metrics/mod.rs`

- [ ] **Create `src/metrics/mod.rs`**

```rust
//! Lightweight Prometheus metrics for modelrouter.
//!
//! Submodule `prometheus` contains the axum handler for `GET /metrics`.
pub mod prometheus;

//!
//! Enabled via `--features prometheus`. Provides `AppMetrics` which holds
//! Prometheus counters/gauges exposed at `GET /metrics`.

#[cfg(feature = "prometheus")]
use prometheus::{Counter, CounterVec, Opts, Registry};

#[cfg(feature = "prometheus")]
pub struct AppMetrics {
    pub registry: Registry,
    pub requests_total: CounterVec,
    pub tokens_total: CounterVec,
    pub cost_usd_total: CounterVec,
}

#[cfg(feature = "prometheus")]
impl AppMetrics {
    pub fn new() -> anyhow::Result<Self> {
        let registry = Registry::new();

        let requests_total = CounterVec::new(
            Opts::new("modelrouter_requests_total", "Total proxy requests")
                .namespace("modelrouter"),
            &["model", "provider", "status"],
        )?;
        registry.register(Box::new(requests_total.clone()))?;

        let tokens_total = CounterVec::new(
            Opts::new("modelrouter_tokens_total", "Total tokens processed")
                .namespace("modelrouter"),
            &["model", "provider", "direction"],
        )?;
        registry.register(Box::new(tokens_total.clone()))?;

        let cost_usd_total = CounterVec::new(
            Opts::new("modelrouter_cost_usd_total", "Total cost in USD")
                .namespace("modelrouter"),
            &["model", "provider"],
        )?;
        registry.register(Box::new(cost_usd_total.clone()))?;

        Ok(Self { registry, requests_total, tokens_total, cost_usd_total })
    }

    pub fn record_request(&self, model: &str, provider: &str, status: &str) {
        self.requests_total
            .with_label_values(&[model, provider, status])
            .inc();
    }

    pub fn record_tokens(&self, model: &str, provider: &str, prompt: u32, completion: u32) {
        self.tokens_total
            .with_label_values(&[model, provider, "prompt"])
            .inc_by(prompt as f64);
        self.tokens_total
            .with_label_values(&[model, provider, "completion"])
            .inc_by(completion as f64);
    }

    pub fn record_cost(&self, model: &str, provider: &str, cost: f64) {
        self.cost_usd_total
            .with_label_values(&[model, provider])
            .inc_by(cost);
    }
}
```

### Step 5.4: Create `src/api/routes/prometheus.rs`

```rust
#[cfg(feature = "prometheus")]
pub async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<crate::api::app::AppState>,
) -> impl axum::response::IntoResponse {
    use prometheus::Encoder;
    use axum::http::{header, StatusCode};

    let Some(ref metrics) = state.app_metrics else {
        return (StatusCode::SERVICE_UNAVAILABLE, "metrics not enabled").into_response();
    };

    let encoder = prometheus::TextEncoder::new();
    let metric_families = metrics.registry.gather();
    let mut buf = Vec::new();
    encoder.encode(&metric_families, &mut buf).unwrap_or_default();

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        buf,
    ).into_response()
}
```

### Step 5.5: Add `metrics` module to `src/lib.rs`

```rust
pub mod metrics;
```

### Step 5.6: Add `app_metrics` to `AppState` and wire route

- [ ] **Modify `src/api/app.rs`**

Add `app_metrics` field to `AppState`:
```rust
#[cfg(feature = "prometheus")]
pub app_metrics: Option<Arc<crate::metrics::AppMetrics>>,
#[cfg(not(feature = "prometheus"))]
pub app_metrics: Option<()>,
```

In `build_router()`, add:
```rust
#[cfg(feature = "prometheus")]
{
    use crate::api::routes::prometheus::metrics_handler;
    router = router.route("/metrics", axum::routing::get(metrics_handler));
}
```

**Note:** Add `let mut router = axum::Router::new()` pattern and build up incrementally if the current code chains everything — or add the feature-gated route using `if cfg!(feature = "prometheus")` is not supported at runtime; use a conditional compile block instead.

**Simpler approach:** Always add the `/metrics` route but return 404 when metrics aren't initialised:

Always add to `build_router()`:
```rust
.route("/metrics", get(metrics_handler))
```

And define `metrics_handler` unconditionally:
```rust
// src/api/routes/prometheus.rs
pub async fn metrics_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    #[cfg(feature = "prometheus")]
    {
        use prometheus::Encoder;
        if let Some(ref metrics) = state.app_metrics {
            let encoder = prometheus::TextEncoder::new();
            let families = metrics.registry.gather();
            let mut buf = Vec::new();
            encoder.encode(&families, &mut buf).unwrap_or_default();
            return (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                buf,
            ).into_response();
        }
    }
    axum::http::StatusCode::NOT_FOUND.into_response()
}
```

### Step 5.7: Record metrics in completions handler

- [ ] **Modify `src/api/routes/completions.rs`** — after computing `cost` in the non-streaming path, add:

```rust
#[cfg(feature = "prometheus")]
if let Some(ref metrics) = state.app_metrics {
    metrics.record_request(&canonical_model, &provider_name, "ok");
    metrics.record_tokens(&canonical_model, &provider_name, result.prompt_tokens, result.completion_tokens);
    metrics.record_cost(&canonical_model, &provider_name, cost);
}
```

### Step 5.8: Initialise `app_metrics` at startup

- [ ] **In `src/cli/mod.rs`** (or wherever `AppState` is constructed), add:

```rust
#[cfg(feature = "prometheus")]
let app_metrics = Some(Arc::new(
    crate::metrics::AppMetrics::new().expect("Failed to init Prometheus metrics")
));
#[cfg(not(feature = "prometheus"))]
let app_metrics: Option<()> = None;

// Include in AppState { ... app_metrics, ... }
```

### Step 5.9: Run tests

```bash
cargo test --features prometheus test_metrics 2>&1 | tail -15
```

Expected: `test_metrics_endpoint_returns_200` passes.

```bash
cargo test test_metrics 2>&1 | tail -10
```

Expected: `test_metrics_endpoint_not_present_without_feature` passes (404 returned).

```bash
cargo test --features prometheus 2>&1 | tail -15
```

Expected: all tests pass with prometheus feature enabled.

```bash
cargo test 2>&1 | tail -10
```

Expected: all tests pass without prometheus feature.

### Step 5.10: Commit

```bash
git add src/metrics/ src/api/routes/prometheus.rs src/api/app.rs \
        src/api/routes/completions.rs src/lib.rs Cargo.toml \
        src/cli/mod.rs tests/test_prometheus.rs
git commit -m "feat: GET /metrics Prometheus endpoint (--features prometheus)"
```

---

## Final Verification

- [ ] **Full test suite passes**

```bash
cargo test 2>&1 | tail -20
```

Expected: 0 failures.

- [ ] **All features build**

```bash
cargo build --features prometheus 2>&1 | tail -5
cargo build --features otel 2>&1 | tail -5
cargo build --features postgres 2>&1 | tail -5
cargo build --release 2>&1 | tail -5
```

Expected: all compile cleanly.

- [ ] **Update `PHASES_SUMMARY.md`** — mark Phase 10 as ✅ Complete

- [ ] **Final commit**

```bash
git add docs/plans/PHASES_SUMMARY.md
git commit -m "docs: mark Phase 10 as complete"
git push
```
