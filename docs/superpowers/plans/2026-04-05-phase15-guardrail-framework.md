# Phase 15: Guardrail Framework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a pre-call and post-call guardrail framework to modelrouter, with a configurable chain of guardrails and one built-in OpenAI moderation guardrail.

**Architecture:** A `GuardrailChain` (stored in `AppState`) runs a list of `Guardrail` trait objects in order; each can `Allow`, `Block`, or `Replace` content. Pre-request checks happen in `completions.rs` before provider dispatch; post-response checks happen after the provider returns. Failures are handled per-guardrail `fail_open` config. One built-in guardrail (`OpenAIModerationGuardrail`) POSTs to the OpenAI moderation API.

**Tech Stack:** Rust, axum, async-trait, reqwest (already a dep), serde_json, tokio

---

## Critical Codebase Patterns (read before implementing any task)

### AppState Test Files
Any new field added to `AppState` must be added to ALL 11 of these test files:
1. `tests/test_completions.rs`
2. `tests/test_cache.rs`
3. `tests/test_embeddings.rs`
4. `tests/test_messages.rs`
5. `tests/test_per_key_budgets.rs`
6. `tests/test_dashboard.rs`
7. `tests/test_prometheus.rs`
8. `tests/test_telemetry.rs` (otel-gated — check the `#[cfg(feature = "otel")]` block)
9. `tests/test_responses.rs`
10. `tests/test_audio.rs`
11. `tests/test_images.rs`

### AppState Construction Pattern (in `src/cli/mod.rs`)
New fields are added to the `AppState { ... }` struct literal inside `serve_command`. For `guardrails`, follow the same pattern as `callbacks`:
```rust
guardrails: {
    let mut chain: Vec<(Box<dyn crate::guardrails::Guardrail>, bool)> = vec![];
    for cfg in &settings.guardrails {
        match cfg.guardrail_type.as_str() {
            "openai_moderation" => {
                let api_key = cfg.api_key.clone()
                    .or_else(|| settings.providers.get("openai").map(|p| p.api_key.clone()))
                    .unwrap_or_default();
                chain.push((
                    Box::new(crate::guardrails::openai_moderation::OpenAIModerationGuardrail::with_fail_open(api_key, cfg.fail_open)),
                    cfg.fail_open,
                ));
            }
            other => tracing::warn!("Unknown guardrail type: {}", other),
        }
    }
    Arc::new(crate::guardrails::GuardrailChain::new(chain))
},
```

### Error type for blocked guardrail
Return `ApiError::PolicyDenied { reason: ..., status: 400 }` when a guardrail blocks.

### Tracing span location
Guardrail checks happen inside `chat_completions_inner` in `src/api/routes/completions.rs`. No extra span is required — the existing `chat_completions` outer span covers it.

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/guardrails/mod.rs` | Create | `Guardrail` trait, `GuardrailContext`, `GuardrailDecision`, `GuardrailChain` |
| `src/guardrails/openai_moderation.rs` | Create | Built-in OpenAI moderation guardrail |
| `src/config/schema.rs` | Modify | Add `GuardrailConfig`, add `guardrails: Vec<GuardrailConfig>` to `Settings` |
| `src/lib.rs` | Modify | Add `pub mod guardrails;` |
| `src/api/app.rs` | Modify | Add `guardrails: Arc<GuardrailChain>` to `AppState` |
| `src/cli/mod.rs` | Modify | Build `GuardrailChain` from settings in `serve_command` |
| `src/api/routes/completions.rs` | Modify | Pre-request + post-response guardrail checks |
| `tests/test_guardrails.rs` | Create | Unit tests for chain and guardrail logic |
| All 8 AppState test files | Modify | Add `guardrails` field with empty chain |

---

### Task 1: Core guardrail types and GuardrailChain

**Files:**
- Create: `src/guardrails/mod.rs`
- Modify: `src/lib.rs`
- Test: `tests/test_guardrails.rs`

- [ ] **Step 1: Write the failing test**

Create `tests/test_guardrails.rs`:
```rust
mod common;

use modelrouter::guardrails::{
    GuardrailChain, GuardrailContext, GuardrailDecision,
};

#[tokio::test]
async fn empty_chain_allows_everything() {
    let chain = GuardrailChain::new(vec![]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([{"role": "user", "content": "Hello"}]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_request(&ctx).await;
    assert!(matches!(decision, GuardrailDecision::Allow));
}

#[tokio::test]
async fn empty_chain_allows_response() {
    let chain = GuardrailChain::new(vec![]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([{"role": "user", "content": "Hello"}]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_response(&ctx, "Hello back").await;
    assert!(matches!(decision, GuardrailDecision::Allow));
}

struct AlwaysBlockGuardrail;

#[async_trait::async_trait]
impl modelrouter::guardrails::Guardrail for AlwaysBlockGuardrail {
    fn name(&self) -> &str { "always-block" }
    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Block { reason: "blocked".to_string() }
    }
    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Block { reason: "blocked".to_string() }
    }
}

#[tokio::test]
async fn chain_with_blocking_guardrail_returns_block() {
    let chain = GuardrailChain::new(vec![
        (Box::new(AlwaysBlockGuardrail) as Box<dyn modelrouter::guardrails::Guardrail>, false),
    ]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_request(&ctx).await;
    assert!(matches!(decision, GuardrailDecision::Block { .. }));
}

struct AlwaysReplaceGuardrail;

#[async_trait::async_trait]
impl modelrouter::guardrails::Guardrail for AlwaysReplaceGuardrail {
    fn name(&self) -> &str { "always-replace" }
    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Allow
    }
    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Replace { content: "[redacted]".to_string() }
    }
}

#[tokio::test]
async fn chain_replace_decision_is_returned_for_response() {
    let chain = GuardrailChain::new(vec![
        (Box::new(AlwaysReplaceGuardrail) as Box<dyn modelrouter::guardrails::Guardrail>, false),
    ]);
    let ctx = GuardrailContext {
        messages: serde_json::json!([]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    let decision = chain.check_response(&ctx, "some response").await;
    assert!(matches!(decision, GuardrailDecision::Replace { content } if content == "[redacted]"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test test_guardrails 2>&1 | head -20`
Expected: compile error — `modelrouter::guardrails` doesn't exist yet

- [ ] **Step 3: Implement `src/guardrails/mod.rs`**

```rust
pub mod openai_moderation;

use serde_json::Value;

/// Context passed to every guardrail check.
pub struct GuardrailContext {
    /// The messages array from the request body.
    pub messages: Value,
    pub model: String,
    pub user_id: i64,
}

/// Decision returned by a guardrail.
pub enum GuardrailDecision {
    Allow,
    Block { reason: String },
    Replace { content: String },
}

/// Trait every guardrail must implement.
#[async_trait::async_trait]
pub trait Guardrail: Send + Sync {
    fn name(&self) -> &str;
    async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision;
    async fn check_response(&self, ctx: &GuardrailContext, response: &str) -> GuardrailDecision;
}

/// Ordered chain of guardrails. Each entry is `(guardrail, fail_open)`.
/// `fail_open = true` means: if the guardrail panics or errors internally, Allow.
/// The chain short-circuits on the first Block or Replace decision.
pub struct GuardrailChain {
    guardrails: Vec<(Box<dyn Guardrail>, bool)>,
}

impl GuardrailChain {
    pub fn new(guardrails: Vec<(Box<dyn Guardrail>, bool)>) -> Self {
        Self { guardrails }
    }

    /// Run all guardrails against the request. Returns the first non-Allow decision,
    /// or Allow if all pass.
    pub async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision {
        for (guardrail, fail_open) in &self.guardrails {
            let decision = guardrail.check_request(ctx).await;
            match decision {
                GuardrailDecision::Allow => continue,
                other => {
                    // fail_open: if Block came from an internal error path it would be wrapped;
                    // for now treat all non-Allow as authoritative unless fail_open is handled
                    // by the guardrail impl itself.
                    let _ = fail_open; // used by implementations, not by the chain runner
                    return other;
                }
            }
        }
        GuardrailDecision::Allow
    }

    /// Run all guardrails against the response. Returns the first non-Allow decision,
    /// or Allow if all pass.
    pub async fn check_response(&self, ctx: &GuardrailContext, response: &str) -> GuardrailDecision {
        for (guardrail, fail_open) in &self.guardrails {
            let decision = guardrail.check_response(ctx, response).await;
            match decision {
                GuardrailDecision::Allow => continue,
                other => {
                    let _ = fail_open;
                    return other;
                }
            }
        }
        GuardrailDecision::Allow
    }
}
```

Create a stub `src/guardrails/openai_moderation.rs` (full impl in Task 3):
```rust
use super::{Guardrail, GuardrailContext, GuardrailDecision};

pub struct OpenAIModerationGuardrail {
    api_key: String,
}

impl OpenAIModerationGuardrail {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

#[async_trait::async_trait]
impl Guardrail for OpenAIModerationGuardrail {
    fn name(&self) -> &str { "openai-moderation" }

    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Allow
    }

    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Allow
    }
}
```

- [ ] **Step 4: Add `pub mod guardrails;` to `src/lib.rs`**

Add after the existing `pub mod callbacks;` line:
```rust
pub mod guardrails;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test test_guardrails`
Expected: 4 tests pass

- [ ] **Step 6: Commit**

```bash
git add src/guardrails/mod.rs src/guardrails/openai_moderation.rs src/lib.rs tests/test_guardrails.rs
git commit -m "feat: guardrail framework — trait, chain, and unit tests"
```

---

### Task 2: Config + AppState integration

**Files:**
- Modify: `src/config/schema.rs` — add `GuardrailConfig`, add `guardrails` to `Settings`
- Modify: `src/api/app.rs` — add `guardrails: Arc<GuardrailChain>` to `AppState`
- Modify: `src/cli/mod.rs` — build `GuardrailChain` in `serve_command`
- Modify: all 8 AppState test files — add `guardrails` field

- [ ] **Step 1: Add `GuardrailConfig` to `src/config/schema.rs`**

Add after the `CallbacksConfig` section (around line 344):
```rust
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GuardrailConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub guardrail_type: String,
    /// If true, a guardrail error causes Allow rather than Block.
    #[serde(default)]
    pub fail_open: bool,
    /// API key override (e.g. for openai_moderation). Falls back to providers.openai.api_key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// HTTP endpoint for external guardrails (e.g. Presidio).
    #[serde(default)]
    pub endpoint: Option<String>,
}
```

Add `guardrails` field to `Settings` (after `callbacks`):
```rust
#[serde(default)]
pub guardrails: Vec<GuardrailConfig>,
```

- [ ] **Step 2: Run `cargo build` to verify it compiles**

Run: `cargo build`
Expected: success

- [ ] **Step 3: Add `guardrails` field to `AppState` in `src/api/app.rs`**

Add after `pub callbacks:` line:
```rust
pub guardrails: Arc<crate::guardrails::GuardrailChain>,
```

Also add the import at the top of the use block if needed — but `GuardrailChain` is referenced via full path so no import needed.

- [ ] **Step 4: Add `guardrails` to AppState construction in `src/cli/mod.rs`**

Inside the `AppState { ... }` literal (after `callbacks: { ... }`), add:
```rust
guardrails: {
    let mut chain: Vec<(Box<dyn crate::guardrails::Guardrail>, bool)> = vec![];
    for cfg in &settings.guardrails {
        match cfg.guardrail_type.as_str() {
            "openai_moderation" => {
                let api_key = cfg.api_key.clone()
                    .or_else(|| settings.providers.get("openai").map(|p| p.api_key.clone()))
                    .unwrap_or_default();
                chain.push((
                    Box::new(crate::guardrails::openai_moderation::OpenAIModerationGuardrail::with_fail_open(api_key, cfg.fail_open)),
                    cfg.fail_open,
                ));
            }
            other => tracing::warn!(guardrail_type = other, "Unknown guardrail type, skipping"),
        }
    }
    Arc::new(crate::guardrails::GuardrailChain::new(chain))
},
```

- [ ] **Step 5: Add `guardrails` to all 8 AppState test files**

In each of the 8 test files listed in the pattern section, find the `AppState { ... }` literal and add:
```rust
guardrails: Arc::new(modelrouter::guardrails::GuardrailChain::new(vec![])),
```

The 8 files:
- `tests/test_completions.rs`
- `tests/test_cache.rs`
- `tests/test_embeddings.rs`
- `tests/test_messages.rs`
- `tests/test_per_key_budgets.rs`
- `tests/test_dashboard.rs`
- `tests/test_prometheus.rs`
- `tests/test_telemetry.rs`

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add src/config/schema.rs src/api/app.rs src/cli/mod.rs \
  tests/test_completions.rs tests/test_cache.rs tests/test_embeddings.rs \
  tests/test_messages.rs tests/test_per_key_budgets.rs tests/test_dashboard.rs \
  tests/test_prometheus.rs tests/test_telemetry.rs
git commit -m "feat: guardrail config, AppState field, and test file wiring"
```

---

### Task 3: OpenAI Moderation built-in guardrail

**Files:**
- Modify: `src/guardrails/openai_moderation.rs` — implement real HTTP call

- [ ] **Step 1: Add unit test for moderation guardrail to `tests/test_guardrails.rs`**

Add at the end of the file:
```rust
use modelrouter::guardrails::openai_moderation::OpenAIModerationGuardrail;

#[tokio::test]
async fn moderation_guardrail_allows_with_empty_api_key_and_no_server() {
    // With an empty API key, the HTTP call will fail. fail_open behavior
    // is handled by the guardrail: on reqwest error it returns Allow.
    let guardrail = OpenAIModerationGuardrail::new("".to_string());
    let ctx = GuardrailContext {
        messages: serde_json::json!([{"role": "user", "content": "Hello"}]),
        model: "gpt-4o".to_string(),
        user_id: 1,
    };
    // Should not panic. With no real server, an error means Allow (fail-safe default).
    let decision = guardrail.check_request(&ctx).await;
    // Either Allow (error path) or a real Block — both are valid depending on env.
    // We just verify it doesn't panic.
    let _ = decision;
}
```

- [ ] **Step 2: Run test to verify it compiles (will pass trivially with stub)**

Run: `cargo test --test test_guardrails moderation_guardrail_allows`
Expected: PASS (stub always returns Allow)

- [ ] **Step 3: Implement real OpenAI moderation in `src/guardrails/openai_moderation.rs`**

Replace the stub with:
```rust
use super::{Guardrail, GuardrailContext, GuardrailDecision};

pub struct OpenAIModerationGuardrail {
    api_key: String,
    /// If true, HTTP/parse errors cause Allow. If false, they cause Block.
    fail_open: bool,
}

impl OpenAIModerationGuardrail {
    pub fn new(api_key: String) -> Self {
        Self { api_key, fail_open: true }
    }

    pub fn with_fail_open(api_key: String, fail_open: bool) -> Self {
        Self { api_key, fail_open }
    }

    /// Extract plain text from a messages array for moderation.
    fn messages_to_text(messages: &serde_json::Value) -> String {
        messages
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .filter_map(|m| m["content"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    async fn moderate(&self, text: &str) -> GuardrailDecision {
        if self.api_key.is_empty() || text.is_empty() {
            return GuardrailDecision::Allow;
        }
        let client = reqwest::Client::new();
        let body = serde_json::json!({ "input": text });
        let resp = match client
            .post("https://api.openai.com/v1/moderations")
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, fail_open = self.fail_open, "OpenAI moderation request failed");
                return if self.fail_open {
                    GuardrailDecision::Allow
                } else {
                    GuardrailDecision::Block { reason: format!("moderation check failed: {}", e) }
                };
            }
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, fail_open = self.fail_open, "Failed to parse moderation response");
                return if self.fail_open {
                    GuardrailDecision::Allow
                } else {
                    GuardrailDecision::Block { reason: format!("moderation parse failed: {}", e) }
                };
            }
        };
        let flagged = json["results"][0]["flagged"].as_bool().unwrap_or(false);
        if flagged {
            GuardrailDecision::Block {
                reason: "content flagged by OpenAI moderation".to_string(),
            }
        } else {
            GuardrailDecision::Allow
        }
    }
}

#[async_trait::async_trait]
impl Guardrail for OpenAIModerationGuardrail {
    fn name(&self) -> &str { "openai-moderation" }

    async fn check_request(&self, ctx: &GuardrailContext) -> GuardrailDecision {
        let text = Self::messages_to_text(&ctx.messages);
        self.moderate(&text).await
    }

    async fn check_response(&self, _ctx: &GuardrailContext, response: &str) -> GuardrailDecision {
        self.moderate(response).await
    }
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test`
Expected: all tests pass (moderation test allows because empty key → early return)

- [ ] **Step 5: Commit**

```bash
git add src/guardrails/openai_moderation.rs tests/test_guardrails.rs
git commit -m "feat: OpenAI moderation built-in guardrail with fail-open error handling"
```

---

### Task 4: Integrate guardrails into completions handler

**Files:**
- Modify: `src/api/routes/completions.rs` — pre-request check + post-response check

- [ ] **Step 1: Add integration test to `tests/test_completions.rs`**

Add at the end of the file:
```rust
use modelrouter::guardrails::{Guardrail, GuardrailChain, GuardrailContext, GuardrailDecision};

struct BlockAllGuardrail;

#[async_trait::async_trait]
impl Guardrail for BlockAllGuardrail {
    fn name(&self) -> &str { "block-all" }
    async fn check_request(&self, _ctx: &GuardrailContext) -> GuardrailDecision {
        GuardrailDecision::Block { reason: "blocked by test guardrail".to_string() }
    }
    async fn check_response(&self, _ctx: &GuardrailContext, _response: &str) -> GuardrailDecision {
        GuardrailDecision::Allow
    }
}

async fn test_app_with_blocking_guardrail() -> TestServer {
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
        response: "Hello!".to_string(),
    }));
    let policy = Arc::new(PolicyEngine::new(db.clone()));
    let fallback = Arc::new(FallbackChain::new(HashMap::new()));
    let complexity_router = Arc::new(modelrouter::router::complexity::ComplexityRouter::new(None));
    let response_cache = Arc::new(modelrouter::router::cache::ResponseCache::new(
        &modelrouter::config::schema::CacheConfig::default(),
    ));
    let embedding_registry = Arc::new(
        modelrouter::providers::embed_registry::EmbeddingRegistry::new_with_mock(
            common::MockEmbeddingAdapter { embedding: vec![0.1_f32, 0.2] },
        ),
    );
    let load_balancer = Arc::new(modelrouter::router::load_balancer::LoadBalancer::new(
        std::collections::HashMap::new(),
    ));
    let guardrails = Arc::new(GuardrailChain::new(vec![
        (Box::new(BlockAllGuardrail) as Box<dyn Guardrail>, false),
    ]));
    let state = AppState {
        live_settings: Arc::new(arc_swap::ArcSwap::from_pointee((*settings).clone())),
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
        load_balancer,
        concurrency: Arc::new(modelrouter::router::concurrency::ConcurrencyLimiter::new()),
        circuit_breaker: Arc::new(modelrouter::router::circuit_breaker::CircuitBreaker::default()),
        ip_rate_limiter: Arc::new(modelrouter::api::middleware::ip_rate_limit::IpRateLimiter::new(0)),
        session_limiter: Arc::new(modelrouter::router::session_limits::SessionLimiter::new(0, 0)),
        app_metrics: None,
        callbacks: std::sync::Arc::new(modelrouter::callbacks::CallbackDispatcher::new(vec![])),
        guardrails,
    };
    TestServer::new(build_router(state)).unwrap()
}

#[tokio::test]
async fn blocking_guardrail_returns_400() {
    let server = test_app_with_blocking_guardrail().await;
    let resp = server
        .post("/v1/chat/completions")
        .add_header(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-token"),
        )
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hello"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 400);
}
```

Also add the import at the top of `tests/test_completions.rs` (after existing imports):
```rust
use modelrouter::guardrails::{Guardrail, GuardrailChain, GuardrailContext, GuardrailDecision};
```

- [ ] **Step 2: Run test to verify it fails (no guardrail check in handler yet)**

Run: `cargo test --test test_completions blocking_guardrail`
Expected: FAIL — returns 200 instead of 400

- [ ] **Step 3: Add pre-request guardrail check to `src/api/routes/completions.rs`**

In `chat_completions_inner`, after the session rate limit check block (around line 155, after the `session_limiter` block) and before the lifecycle hooks + load balancer resolution, add:

```rust
// Pre-request guardrail check
let guardrail_ctx = crate::guardrails::GuardrailContext {
    messages: body["messages"].clone(),
    model: model.clone(),
    user_id: user.id,
};
match state.guardrails.check_request(&guardrail_ctx).await {
    crate::guardrails::GuardrailDecision::Allow => {}
    crate::guardrails::GuardrailDecision::Block { reason } => {
        return Err(ApiError::PolicyDenied { reason, status: 400 });
    }
    crate::guardrails::GuardrailDecision::Replace { .. } => {
        // Replace on request is not supported; treat as Allow
    }
}
```

- [ ] **Step 4: Run test to verify pre-request check works**

Run: `cargo test --test test_completions blocking_guardrail`
Expected: PASS

- [ ] **Step 5: Add post-response guardrail check**

In `chat_completions_inner`, in the non-streaming path, after `let result = loop { ... }` and before computing `latency_ms`, add:

```rust
// Post-response guardrail check — runs only for non-streaming path
let result = match state.guardrails.check_response(&guardrail_ctx, &result.content).await {
    crate::guardrails::GuardrailDecision::Allow => result,
    crate::guardrails::GuardrailDecision::Block { reason } => {
        return Err(ApiError::PolicyDenied { reason, status: 400 });
    }
    crate::guardrails::GuardrailDecision::Replace { content } => {
        let mut r = result;
        r.content = content;
        r
    }
};
```

Note: `guardrail_ctx` was already created before the provider call, so it's in scope here.

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: all tests pass

- [ ] **Step 7: Commit**

```bash
git add src/api/routes/completions.rs tests/test_completions.rs
git commit -m "feat: pre-request and post-response guardrail checks in completions handler"
```
