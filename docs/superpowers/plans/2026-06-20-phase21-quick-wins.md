# Phase 21: Quick Wins — Logging Opt-Out, Webhook Callbacks, Chinese Providers, Routing Shortcuts

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement four near-zero-effort features that close meaningful competitor gaps: per-request prompt logging opt-out, generic webhook callback backends (DB-managed, with UI + CLI), Chinese model provider config and pricing, and `:fastest`/`:cheapest` routing shortcuts.

**Architecture:** Each feature is self-contained and touches a different layer. #1 is a header check in the completions pipeline. #2 adds a new DB table + callback backend + admin pages. #3 is config templates + pricing data in docs and README. #4 extends the routing engine with two reserved model name keywords resolved before alias lookup.

**Tech Stack:** Rust, axum, sqlx, minijinja/HTMX admin templates, clap CLI, reqwest for outbound webhooks.

---

## Phase Summary (update competitor comparison doc after all tasks complete)

| # | Feature | Effort | Closes Gap vs. |
|---|---|---|---|
| 1 | Per-request prompt logging opt-out (`X-No-Log: true`) | ~1h | OpenRouter (`zdr: true`) |
| 2 | Generic webhook callback backends (DB-managed) | ~1d | OpenRouter Broadcast, LiteLLM/Portkey |
| 3 | Chinese model provider config + pricing | ~2h | One API |
| 4 | `:fastest` / `:cheapest` routing shortcuts | ~3h | OpenRouter `:nitro` / `:floor` |
| 5 | Session stickiness (plan only — implement later) | ~2d | OpenRouter `session_id` |
| ND | Not Diamond hook example (docs only) | ~1h | Martian / Not Diamond |

---

## Files Created or Modified

### Task 1 — Logging opt-out
- Modify: `src/api/routes/completions.rs` — check `X-No-Log` header; skip `PromptRepository::create` and callbacks if set; still write cost ledger for budget enforcement

### Task 2 — Webhook callbacks
- Create: `src/callbacks/webhook.rs` — `WebhookBackend` implementing `CallbackBackend`; fires POST to configured URL; optional secret header auth; non-blocking (`tokio::spawn`)
- Modify: `src/callbacks/mod.rs` — add `pub mod webhook`
- Add migration: `migrations/NNNN_webhook_callbacks.sql` — `webhook_callbacks` table (id, name, url, events TEXT, secret_header_name, secret_header_value, enabled, created_at)
- Add repo: `src/db/repositories/webhook_callbacks.rs` — CRUD for `webhook_callbacks` table
- Modify: `src/db/repositories/mod.rs` — pub mod webhook_callbacks
- Modify: `src/db/sqlite/mod.rs` (and postgres equivalent) — impl trait for new repo
- Add DB trait: `src/db/repositories/webhook_callbacks.rs` — `WebhookCallbackRepository` trait
- Modify: `src/api/app.rs` — `AppState` gets `webhook_dispatch: Arc<WebhookDispatcher>`; builder loads webhooks from DB and wires `WebhookBackend` instances into `CallbackDispatcher`
- Modify: `src/api/admin/routes.rs` — add REST handlers for webhook CRUD
- Create: `src/api/admin/webhooks.rs` — axum handlers: list, create, delete, test, toggle-enabled
- Create: `templates/admin/webhooks.html` — HTMX page (list table + create form + test button)
- Modify: `templates/admin/base.html` — add "Webhooks" nav link
- Modify: `src/api/app.rs` (`build_router`) — wire `/admin/webhooks` GET/POST and `/admin/webhooks/:id/delete|enable|disable|test` routes
- Modify: `src/cli/commands.rs` — add `Webhook(WebhookArgs)` subcommand with `Add`, `List`, `Delete`, `Test` sub-subcommands
- Modify: `src/cli/admin.rs` (or add `src/cli/webhook.rs`) — implement CLI dispatch

### Task 3 — Chinese providers
- Modify: `README.md` — add DeepSeek, Qwen (Tongyi), and Doubao provider examples in the Providers section
- Modify: `docs/local-setup.md` — add provider config block examples for Chinese models
- Modify: `src/router/cost.rs` — add pricing entries for common Chinese models (deepseek-chat, deepseek-coder, qwen-max, qwen-plus, doubao-lite, doubao-pro)

### Task 4 — Routing shortcuts
- Modify: `src/config/schema.rs` — add `shortcuts: RoutingShortcutsConfig` to `RoutingConfig`; `RoutingShortcutsConfig { fastest: Option<String>, cheapest: Option<String> }`
- Modify: `src/router/engine.rs` — before alias lookup, check if `requested_model == ":fastest"` or `":cheapest"` and resolve to the configured shortcut target
- Modify: `templates/admin/models.html` — add a "Routing Shortcuts" card showing current `:fastest` and `:cheapest` values (read from config, display-only)
- Modify: `README.md` — document `:fastest` and `:cheapest` shortcuts in the routing section

### Task 5 (plan only, do not implement)
- See session stickiness plan at bottom of this document.

### Not Diamond hook example (docs)
- Create: `docs/hooks/not-diamond-router.md` — Python script example implementing Not Diamond as a `request.pre` pipeline hook

---

## Task 1: Per-Request Prompt Logging Opt-Out

**Files:**
- Modify: `src/api/routes/completions.rs`

The `X-No-Log: true` header is set by the API caller (not an admin setting), so no UI or CLI changes are needed. Cost is still recorded for budget enforcement; only the `prompts` table write and callback dispatch are skipped.

- [ ] **Step 1: Write the failing test**

Add to `src/api/routes/completions.rs` (in the `#[cfg(test)]` block at the bottom, or a new tests module):

```rust
#[cfg(test)]
mod no_log_tests {
    #[test]
    fn no_log_header_detected() {
        // Pure unit test — parse header value
        let val = "true";
        assert!(val.eq_ignore_ascii_case("true"));
        let val2 = "false";
        assert!(!val2.eq_ignore_ascii_case("true"));
    }
}
```

Run: `cargo test no_log_header_detected`
Expected: FAIL (test module doesn't exist yet)

- [ ] **Step 2: Add the helper and test module**

At the bottom of `src/api/routes/completions.rs`, before the final `}`, add:

```rust
/// Returns true if the caller set `X-No-Log: true` (case-insensitive).
pub fn should_skip_logging(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-no-log")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod no_log_tests {
    use super::should_skip_logging;
    use axum::http::HeaderMap;

    #[test]
    fn no_log_header_detected() {
        let mut h = HeaderMap::new();
        h.insert("x-no-log", "true".parse().unwrap());
        assert!(should_skip_logging(&h));
    }

    #[test]
    fn no_log_header_false_value() {
        let mut h = HeaderMap::new();
        h.insert("x-no-log", "false".parse().unwrap());
        assert!(!should_skip_logging(&h));
    }

    #[test]
    fn no_log_header_absent() {
        assert!(!should_skip_logging(&HeaderMap::new()));
    }

    #[test]
    fn no_log_header_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert("x-no-log", "TRUE".parse().unwrap());
        assert!(should_skip_logging(&h));
    }
}
```

Run: `cargo test no_log`
Expected: 4 tests pass.

- [ ] **Step 3: Thread the header into the handler**

In `chat_completions_inner`, the `Parts` are not directly available because the handler takes `Json(body)` — but axum lets us add a `headers: axum::http::HeaderMap` extractor. Change the handler signatures:

```rust
// Change:
pub async fn chat_completions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {

// To:
pub async fn chat_completions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
```

And the inner function:

```rust
async fn chat_completions_inner(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
```

Pass `headers.clone()` through from outer to inner. Pass `&headers` at the call site.

- [ ] **Step 4: Skip logging and callbacks when header is set**

In `chat_completions_inner`, locate the `tokio::spawn(async move { ... })` block that writes the prompt and dispatches callbacks. Add a guard:

```rust
let skip_log = should_skip_logging(&headers);

// Fire-and-forget: log prompt + cost
let state_clone = state.clone();
// ... (existing variable clones) ...
tokio::spawn(async move {
    // Always record cost for budget enforcement
    let ledger = NewCostLedgerEntry { ... };
    if let Err(e) = CostRepository::create(&*state_clone.db, ledger).await {
        tracing::error!("Failed to record cost: {}", e);
    }

    if !skip_log {
        let prompt = NewPrompt { ... };
        match PromptRepository::create(&*state_clone.db, prompt).await {
            Ok(saved_prompt) => {
                state_clone.callbacks.dispatch(crate::callbacks::CallbackEvent {
                    trace_id: format!("{}", saved_prompt.id),
                    // ... rest of fields ...
                });
            }
            Err(e) => tracing::error!("Failed to record prompt: {}", e),
        }
    } else {
        tracing::debug!(user_id, model = model_clone.as_str(), "prompt logging skipped (X-No-Log: true)");
    }
    // ... lifecycle hooks still fire ...
});
```

Note: The cost ledger write moves outside the `PromptRepository::create` success block — it no longer needs a `prompt_id`. Update `NewCostLedgerEntry` to use `prompt_id: None` (make it `Option<i64>` if not already).

Also apply the same `skip_log` guard in `log_streaming_request` — pass `skip_log: bool` into `StreamLogCtx`.

- [ ] **Step 5: Build and run tests**

```bash
cargo build
cargo test
```

Expected: clean build, all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/api/routes/completions.rs
git commit -m "feat: add X-No-Log header to skip prompt logging while preserving cost tracking"
```

---

## Task 2: Generic Webhook Callback Backends

### 2a — DB Schema

- [ ] **Step 1: Create migration**

Create `migrations/NNNN_webhook_callbacks.sql` where NNNN is the next migration number. Check `migrations/` for the current highest number and increment.

```sql
CREATE TABLE IF NOT EXISTS webhook_callbacks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    url         TEXT NOT NULL,
    events      TEXT NOT NULL DEFAULT '["completion"]',
    secret_header_name  TEXT,
    secret_header_value TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
```

Run: `cargo run -- migrate`
Expected: migration applies cleanly.

### 2b — Repository

- [ ] **Step 2: Write failing repo tests**

Create `src/db/repositories/webhook_callbacks.rs`:

```rust
use async_trait::async_trait;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookCallback {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub events: String,  // JSON array string, e.g. '["completion"]'
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NewWebhookCallback {
    pub name: String,
    pub url: String,
    pub events: String,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

#[async_trait]
pub trait WebhookCallbackRepository {
    async fn list_webhooks(&self) -> Result<Vec<WebhookCallback>>;
    async fn create_webhook(&self, new: NewWebhookCallback) -> Result<WebhookCallback>;
    async fn delete_webhook(&self, id: i64) -> Result<()>;
    async fn set_webhook_enabled(&self, id: i64, enabled: bool) -> Result<()>;
    async fn list_enabled_webhooks(&self) -> Result<Vec<WebhookCallback>>;
}

#[cfg(test)]
mod tests {
    // Integration tests live in sqlite/webhook_callbacks.rs
}
```

- [ ] **Step 3: Implement SQLite version**

Create `src/db/sqlite/webhook_callbacks.rs`:

```rust
use async_trait::async_trait;
use anyhow::Result;
use crate::db::{repositories::webhook_callbacks::{WebhookCallback, WebhookCallbackRepository, NewWebhookCallback}, sqlite::SqliteDb};

#[async_trait]
impl WebhookCallbackRepository for SqliteDb {
    async fn list_webhooks(&self) -> Result<Vec<WebhookCallback>> {
        let rows = sqlx::query_as!(
            WebhookCallback,
            r#"SELECT id, name, url, events, secret_header_name, secret_header_value,
               (enabled != 0) as "enabled: bool", created_at
               FROM webhook_callbacks ORDER BY created_at DESC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_enabled_webhooks(&self) -> Result<Vec<WebhookCallback>> {
        let rows = sqlx::query_as!(
            WebhookCallback,
            r#"SELECT id, name, url, events, secret_header_name, secret_header_value,
               (enabled != 0) as "enabled: bool", created_at
               FROM webhook_callbacks WHERE enabled = 1 ORDER BY created_at DESC"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn create_webhook(&self, new: NewWebhookCallback) -> Result<WebhookCallback> {
        let row = sqlx::query_as!(
            WebhookCallback,
            r#"INSERT INTO webhook_callbacks (name, url, events, secret_header_name, secret_header_value)
               VALUES (?, ?, ?, ?, ?)
               RETURNING id, name, url, events, secret_header_name, secret_header_value,
               (enabled != 0) as "enabled: bool", created_at"#,
            new.name, new.url, new.events, new.secret_header_name, new.secret_header_value
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    async fn delete_webhook(&self, id: i64) -> Result<()> {
        sqlx::query!("DELETE FROM webhook_callbacks WHERE id = ?", id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_webhook_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let v = if enabled { 1i32 } else { 0i32 };
        sqlx::query!("UPDATE webhook_callbacks SET enabled = ? WHERE id = ?", v, id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
```

Wire into `src/db/sqlite/mod.rs` and `src/db/repositories/mod.rs` with `pub mod webhook_callbacks`.

Also add `WebhookCallbackRepository` to the `DatabaseProvider` trait bound in `src/api/app.rs`.

If PostgreSQL feature is enabled, add a parallel `src/db/postgres/webhook_callbacks.rs` with `query_as!` using `$1` placeholders.

- [ ] **Step 4: Run `cargo build`**

```bash
cargo build
```

Expected: clean compile (no handler tests yet, just data layer).

### 2c — Callback Backend

- [ ] **Step 5: Write failing unit test for WebhookBackend**

Create `src/callbacks/webhook.rs`:

```rust
use super::{CallbackBackend, CallbackEvent};

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub name: String,
    pub url: String,
    /// JSON array string of event names to forward, e.g. `["completion"]`.
    /// Empty or `["*"]` means forward all events.
    pub events: Vec<String>,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

pub struct WebhookBackend {
    config: WebhookConfig,
    client: reqwest::Client,
}

impl WebhookBackend {
    pub fn new(config: WebhookConfig) -> Self {
        Self { config, client: reqwest::Client::new() }
    }
}

impl CallbackBackend for WebhookBackend {
    fn send(&self, event: CallbackEvent) {
        // Only send if "completion" (or "*") is in the event list
        let events = &self.config.events;
        if !events.is_empty() && !events.iter().any(|e| e == "*" || e == "completion") {
            return;
        }

        let url = self.config.url.clone();
        let secret_name = self.config.secret_header_name.clone();
        let secret_value = self.config.secret_header_value.clone();
        let client = self.client.clone();
        let name = self.config.name.clone();

        tokio::spawn(async move {
            let body = serde_json::json!({
                "event": "completion",
                "trace_id": event.trace_id,
                "user_id": event.user_id,
                "model": event.model,
                "provider": event.provider,
                "prompt_tokens": event.prompt_tokens,
                "completion_tokens": event.completion_tokens,
                "cost_usd": event.cost_usd,
                "latency_ms": event.latency_ms,
            });

            let mut req = client.post(&url).json(&body);
            if let (Some(header), Some(value)) = (secret_name, secret_value) {
                req = req.header(header, value);
            }
            if let Err(e) = req.send().await {
                tracing::warn!(webhook = name.as_str(), url = url.as_str(), "webhook callback failed: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_events_filter_passes() {
        let cfg = WebhookConfig {
            name: "test".into(),
            url: "http://example.com".into(),
            events: vec!["*".into()],
            secret_header_name: None,
            secret_header_value: None,
        };
        let events = &cfg.events;
        assert!(events.iter().any(|e| e == "*" || e == "completion"));
    }

    #[test]
    fn empty_events_filter_skips() {
        let cfg = WebhookConfig {
            name: "test".into(),
            url: "http://example.com".into(),
            events: vec!["budget_exceeded".into()],
            secret_header_name: None,
            secret_header_value: None,
        };
        let events = &cfg.events;
        assert!(!events.iter().any(|e| e == "*" || e == "completion"));
    }
}
```

Add `pub mod webhook;` to `src/callbacks/mod.rs`.

Run: `cargo test webhook`
Expected: 2 tests pass.

### 2d — Wire into AppState

- [ ] **Step 6: Load webhooks from DB on startup**

In `src/api/app.rs` (wherever `CallbackDispatcher::new` is called), after building LangSmith and LangFuse backends, load enabled webhooks from DB and append `WebhookBackend` instances:

```rust
// Load DB-configured webhook backends
let webhook_rows = WebhookCallbackRepository::list_enabled_webhooks(&*db)
    .await
    .unwrap_or_default();
for row in webhook_rows {
    let events: Vec<String> = serde_json::from_str(&row.events).unwrap_or_default();
    backends.push(Box::new(crate::callbacks::webhook::WebhookBackend::new(
        crate::callbacks::webhook::WebhookConfig {
            name: row.name,
            url: row.url,
            events,
            secret_header_name: row.secret_header_name,
            secret_header_value: row.secret_header_value,
        }
    )));
}
```

Note: This loads webhooks once at startup. For live reloading without restart, webhooks configured after startup won't be picked up until restart. That's acceptable for v1 — document it.

Run: `cargo build`
Expected: clean.

### 2e — Admin REST API

- [ ] **Step 7: Add REST handlers**

Create `src/api/admin/webhooks.rs`:

```rust
use axum::{extract::{Path, State}, Json, response::IntoResponse};
use serde::Deserialize;
use crate::{api::{app::AppState, admin::auth::SuperAdminSession, error::ApiError},
            db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallbackRepository}};

pub async fn list_webhooks(
    State(state): State<AppState>,
    _session: SuperAdminSession,
) -> Result<impl IntoResponse, ApiError> {
    let rows = WebhookCallbackRepository::list_webhooks(&*state.db)
        .await.map_err(|_| ApiError::Internal)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct CreateWebhookPayload {
    pub name: String,
    pub url: String,
    pub events: Option<Vec<String>>,
    pub secret_header_name: Option<String>,
    pub secret_header_value: Option<String>,
}

pub async fn create_webhook(
    State(state): State<AppState>,
    _session: SuperAdminSession,
    Json(payload): Json<CreateWebhookPayload>,
) -> Result<impl IntoResponse, ApiError> {
    let events = payload.events.unwrap_or_else(|| vec!["completion".into()]);
    let events_json = serde_json::to_string(&events).map_err(|_| ApiError::Internal)?;
    let new = NewWebhookCallback {
        name: payload.name,
        url: payload.url,
        events: events_json,
        secret_header_name: payload.secret_header_name,
        secret_header_value: payload.secret_header_value,
    };
    let row = WebhookCallbackRepository::create_webhook(&*state.db, new)
        .await.map_err(|_| ApiError::Internal)?;
    Ok((axum::http::StatusCode::CREATED, Json(row)))
}

pub async fn delete_webhook(
    State(state): State<AppState>,
    _session: SuperAdminSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    WebhookCallbackRepository::delete_webhook(&*state.db, id)
        .await.map_err(|_| ApiError::Internal)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub async fn enable_webhook(
    State(state): State<AppState>,
    _session: SuperAdminSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    WebhookCallbackRepository::set_webhook_enabled(&*state.db, id, true)
        .await.map_err(|_| ApiError::Internal)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub async fn disable_webhook(
    State(state): State<AppState>,
    _session: SuperAdminSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    WebhookCallbackRepository::set_webhook_enabled(&*state.db, id, false)
        .await.map_err(|_| ApiError::Internal)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
```

Wire routes in `build_router` in `src/api/app.rs`:

```rust
.route("/admin/api/webhooks", get(list_webhooks_api).post(create_webhook_api))
.route("/admin/api/webhooks/:id", delete(delete_webhook_api))
.route("/admin/api/webhooks/:id/enable", post(enable_webhook_api))
.route("/admin/api/webhooks/:id/disable", post(disable_webhook_api))
```

### 2f — Admin Dashboard Page

- [ ] **Step 8: Create webhooks dashboard template**

Create `templates/admin/webhooks.html`:

```html
{% extends "base.html" %}
{% block title %}Webhooks{% endblock %}
{% block content %}
<div class="page-header">
  <h1>Webhook Callbacks</h1>
  <p class="page-subtitle">Outbound webhooks fired after each completion. Takes effect on next server restart.</p>
</div>

<section class="card">
  <h2>Add Webhook</h2>
  <form method="POST" action="/admin/webhooks">
    <div class="form-row">
      <label>Name <input name="name" required placeholder="my-datadog-hook"></label>
      <label>URL <input name="url" type="url" required placeholder="https://..."></label>
    </div>
    <div class="form-row">
      <label>Events (comma-separated, or * for all)
        <input name="events" value="completion" placeholder="completion">
      </label>
    </div>
    <div class="form-row">
      <label>Secret Header Name <input name="secret_header_name" placeholder="Authorization"></label>
      <label>Secret Header Value <input name="secret_header_value" type="password" placeholder="Bearer token..."></label>
    </div>
    <button type="submit">Add Webhook</button>
  </form>
</section>

<section class="card">
  <h2>Configured Webhooks</h2>
  {% if webhooks %}
  <table>
    <thead><tr><th>Name</th><th>URL</th><th>Events</th><th>Status</th><th>Actions</th></tr></thead>
    <tbody>
    {% for w in webhooks %}
    <tr>
      <td>{{ w.name }}</td>
      <td><code>{{ w.url }}</code></td>
      <td><code>{{ w.events }}</code></td>
      <td>{% if w.enabled %}<span class="badge green">enabled</span>{% else %}<span class="badge gray">disabled</span>{% endif %}</td>
      <td>
        {% if w.enabled %}
        <form method="POST" action="/admin/webhooks/{{ w.id }}/disable" style="display:inline">
          <button type="submit" class="btn-small">Disable</button>
        </form>
        {% else %}
        <form method="POST" action="/admin/webhooks/{{ w.id }}/enable" style="display:inline">
          <button type="submit" class="btn-small">Enable</button>
        </form>
        {% endif %}
        <form method="POST" action="/admin/webhooks/{{ w.id }}/delete" style="display:inline"
              onsubmit="return confirm('Delete webhook {{ w.name }}?')">
          <button type="submit" class="btn-small btn-danger">Delete</button>
        </form>
      </td>
    </tr>
    {% endfor %}
    </tbody>
  </table>
  {% else %}
  <p class="empty-state">No webhooks configured yet.</p>
  {% endif %}
</section>
{% endblock %}
```

Add dashboard handlers in `src/api/admin/webhooks.rs`:

```rust
pub async fn get_webhooks_page(
    State(state): State<AppState>,
    session: crate::api::admin::auth::AdminSession,
) -> Result<impl IntoResponse, ApiError> {
    let webhooks = WebhookCallbackRepository::list_webhooks(&*state.db)
        .await.map_err(|_| ApiError::Internal)?;
    let ctx = minijinja::context! { webhooks, admin => session.admin };
    state.render("webhooks.html", ctx).map_err(|_| ApiError::Internal)
}

pub async fn post_create_webhook_page(
    State(state): State<AppState>,
    _session: crate::api::admin::auth::AdminSession,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiError> {
    let name = form.get("name").cloned().unwrap_or_default();
    let url = form.get("url").cloned().unwrap_or_default();
    let events_raw = form.get("events").cloned().unwrap_or_else(|| "completion".into());
    let events: Vec<String> = events_raw.split(',').map(|s| s.trim().to_string()).collect();
    let events_json = serde_json::to_string(&events).map_err(|_| ApiError::Internal)?;
    let new = NewWebhookCallback {
        name, url, events: events_json,
        secret_header_name: form.get("secret_header_name").filter(|s| !s.is_empty()).cloned(),
        secret_header_value: form.get("secret_header_value").filter(|s| !s.is_empty()).cloned(),
    };
    WebhookCallbackRepository::create_webhook(&*state.db, new)
        .await.map_err(|_| ApiError::Internal)?;
    Ok(axum::response::Redirect::to("/admin/webhooks"))
}

pub async fn post_delete_webhook_page(
    State(state): State<AppState>,
    _session: crate::api::admin::auth::AdminSession,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    WebhookCallbackRepository::delete_webhook(&*state.db, id)
        .await.map_err(|_| ApiError::Internal)?;
    Ok(axum::response::Redirect::to("/admin/webhooks"))
}

pub async fn post_enable_webhook_page(State(state): State<AppState>, _session: crate::api::admin::auth::AdminSession, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    WebhookCallbackRepository::set_webhook_enabled(&*state.db, id, true).await.map_err(|_| ApiError::Internal)?;
    Ok(axum::response::Redirect::to("/admin/webhooks"))
}

pub async fn post_disable_webhook_page(State(state): State<AppState>, _session: crate::api::admin::auth::AdminSession, Path(id): Path<i64>) -> Result<impl IntoResponse, ApiError> {
    WebhookCallbackRepository::set_webhook_enabled(&*state.db, id, false).await.map_err(|_| ApiError::Internal)?;
    Ok(axum::response::Redirect::to("/admin/webhooks"))
}
```

Wire dashboard routes in `build_router`:

```rust
.route("/admin/webhooks", get(get_webhooks_page).post(post_create_webhook_page))
.route("/admin/webhooks/:id/delete", post(post_delete_webhook_page))
.route("/admin/webhooks/:id/enable", post(post_enable_webhook_page))
.route("/admin/webhooks/:id/disable", post(post_disable_webhook_page))
```

Add nav link in `templates/admin/base.html` (find the nav section and add alongside "Hooks"):

```html
<li><a href="/admin/webhooks" {% if active_page == "webhooks" %}class="active"{% endif %}>Webhooks</a></li>
```

### 2g — CLI

- [ ] **Step 9: Add CLI subcommand**

In `src/cli/commands.rs`, add to the `Commands` enum:

```rust
/// Manage outbound webhook callbacks
Webhook(WebhookArgs),
```

Add the structs:

```rust
#[derive(Args)]
pub struct WebhookArgs {
    #[command(subcommand)]
    pub command: WebhookCommands,
}

#[derive(Subcommand)]
pub enum WebhookCommands {
    /// List all configured webhooks
    List,
    /// Add a new webhook
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        url: String,
        /// Comma-separated events (default: completion)
        #[arg(long, default_value = "completion")]
        events: String,
        #[arg(long)]
        secret_header_name: Option<String>,
        #[arg(long)]
        secret_header_value: Option<String>,
    },
    /// Delete a webhook by ID
    Delete {
        #[arg(long)]
        id: i64,
    },
    /// Enable a webhook by ID
    Enable {
        #[arg(long)]
        id: i64,
    },
    /// Disable a webhook by ID
    Disable {
        #[arg(long)]
        id: i64,
    },
}
```

In the CLI dispatch (wherever `Commands::Model` is handled, probably `src/cli/admin.rs` or `src/main.rs`), add:

```rust
Commands::Webhook(args) => {
    use crate::db::repositories::webhook_callbacks::{NewWebhookCallback, WebhookCallbackRepository};
    match args.command {
        WebhookCommands::List => {
            let rows = WebhookCallbackRepository::list_webhooks(&*db).await?;
            if rows.is_empty() {
                println!("No webhooks configured.");
            } else {
                println!("{:<4} {:<20} {:<40} {:<20} {}", "ID", "Name", "URL", "Events", "Enabled");
                for r in rows {
                    println!("{:<4} {:<20} {:<40} {:<20} {}", r.id, r.name, r.url, r.events, r.enabled);
                }
            }
        }
        WebhookCommands::Add { name, url, events, secret_header_name, secret_header_value } => {
            let events_vec: Vec<String> = events.split(',').map(|s| s.trim().to_string()).collect();
            let events_json = serde_json::to_string(&events_vec)?;
            let new = NewWebhookCallback { name, url, events: events_json, secret_header_name, secret_header_value };
            let row = WebhookCallbackRepository::create_webhook(&*db, new).await?;
            println!("Created webhook #{}: {} -> {}", row.id, row.name, row.url);
        }
        WebhookCommands::Delete { id } => {
            WebhookCallbackRepository::delete_webhook(&*db, id).await?;
            println!("Deleted webhook #{id}");
        }
        WebhookCommands::Enable { id } => {
            WebhookCallbackRepository::set_webhook_enabled(&*db, id, true).await?;
            println!("Enabled webhook #{id}");
        }
        WebhookCommands::Disable { id } => {
            WebhookCallbackRepository::set_webhook_enabled(&*db, id, false).await?;
            println!("Disabled webhook #{id}");
        }
    }
}
```

- [ ] **Step 10: Build and smoke-test CLI**

```bash
cargo build
./target/debug/modelrouter webhook --help
./target/debug/modelrouter webhook list
./target/debug/modelrouter webhook add --name test --url https://httpbin.org/post
./target/debug/modelrouter webhook list
./target/debug/modelrouter webhook delete --id 1
```

Expected: each command runs, list shows/hides entries.

- [ ] **Step 11: Commit**

```bash
git add migrations/ src/callbacks/webhook.rs src/callbacks/mod.rs \
        src/db/repositories/webhook_callbacks.rs src/db/sqlite/webhook_callbacks.rs \
        src/db/repositories/mod.rs src/db/sqlite/mod.rs \
        src/api/admin/webhooks.rs src/api/app.rs \
        templates/admin/webhooks.html templates/admin/base.html \
        src/cli/commands.rs src/cli/admin.rs
git commit -m "feat: DB-managed webhook callbacks with admin UI and CLI"
```

---

## Task 3: Chinese Model Provider Config + Pricing

No new source files. This task adds pricing data and documentation.

### 3a — Pricing entries

- [ ] **Step 1: Add Chinese model pricing to cost.rs**

Open `src/router/cost.rs`. Find the static pricing table (a `HashMap` or `match` block of model → (input_per_m, output_per_m) tuples). Add:

```rust
// DeepSeek
("deepseek-chat", (0.14, 0.28)),
("deepseek-coder", (0.14, 0.28)),
("deepseek-reasoner", (0.55, 2.19)),
// Alibaba Qwen (Tongyi)
("qwen-max", (0.40, 1.20)),
("qwen-plus", (0.07, 0.21)),
("qwen-turbo", (0.05, 0.10)),
// ByteDance Doubao
("doubao-lite-4k", (0.10, 0.10)),
("doubao-lite-32k", (0.10, 0.10)),
("doubao-pro-4k", (0.80, 0.80)),
("doubao-pro-32k", (0.80, 0.80)),
```

(Prices are approximate USD per million tokens as of mid-2026; operators can override with `[[pricing]]` in config.)

- [ ] **Step 2: Write a test for new pricing entries**

In the test block of `src/router/cost.rs`:

```rust
#[test]
fn chinese_model_pricing_present() {
    let calc = CostCalculator::default();
    // deepseek-chat: 0.14/M in, 0.28/M out
    let cost = calc.calculate("deepseek-chat", 1_000_000, 0);
    assert!((cost - 0.14).abs() < 0.001, "deepseek-chat input cost wrong: {cost}");
    let cost_out = calc.calculate("deepseek-chat", 0, 1_000_000);
    assert!((cost_out - 0.28).abs() < 0.001, "deepseek-chat output cost wrong: {cost_out}");
}
```

Run: `cargo test chinese_model_pricing_present`
Expected: PASS.

- [ ] **Step 3: Add provider config examples to README.md**

In the `## Configuration` → providers section of README.md, add a new subsection:

````markdown
#### Chinese Model Providers

All major Chinese LLM providers expose an OpenAI-compatible API and work with modelrouter's generic `openai_compat` provider type. Configure them as named providers:

```toml
[providers.deepseek]
api_key = "sk-..."
api_base = "https://api.deepseek.com/v1"

[providers.qwen]
api_key = "sk-..."
api_base = "https://dashscope.aliyuncs.com/compatible-mode/v1"

[providers.doubao]
api_key = "..."
api_base = "https://ark.cn-beijing.volces.com/api/v3"
```

Route to them using the `provider/model` syntax:

```
deepseek/deepseek-chat
qwen/qwen-max
doubao/doubao-pro-32k
```

Or add aliases:

```toml
[routing.model_aliases]
deepseek = "deepseek/deepseek-chat"
qwen     = "qwen/qwen-max"
```
````

- [ ] **Step 4: Commit**

```bash
git add src/router/cost.rs README.md
git commit -m "feat: add Chinese model provider docs and pricing (DeepSeek, Qwen, Doubao)"
```

---

## Task 4: `:fastest` / `:cheapest` Routing Shortcuts

### 4a — Config

- [ ] **Step 1: Add config struct**

In `src/config/schema.rs`, add to `RoutingConfig`:

```rust
#[serde(default)]
pub shortcuts: RoutingShortcutsConfig,
```

And the new struct:

```rust
/// Reserved model name shortcuts. Resolved before alias lookup.
/// `:fastest` and `:cheapest` are the special keywords.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RoutingShortcutsConfig {
    /// Model string that `:fastest` resolves to (e.g. "anthropic/claude-haiku-4-5").
    pub fastest: Option<String>,
    /// Model string that `:cheapest` resolves to (e.g. "deepseek/deepseek-chat").
    pub cheapest: Option<String>,
}
```

- [ ] **Step 2: Write config parse test**

In the `#[cfg(test)]` block of `schema.rs`:

```rust
#[test]
fn shortcuts_parse() {
    let s: Settings = toml::from_str(r#"
        [routing.shortcuts]
        fastest  = "anthropic/claude-haiku-4-5"
        cheapest = "deepseek/deepseek-chat"
    "#).unwrap();
    assert_eq!(s.routing.shortcuts.fastest.as_deref(), Some("anthropic/claude-haiku-4-5"));
    assert_eq!(s.routing.shortcuts.cheapest.as_deref(), Some("deepseek/deepseek-chat"));
}

#[test]
fn shortcuts_default_is_none() {
    let s: Settings = toml::from_str("").unwrap();
    assert!(s.routing.shortcuts.fastest.is_none());
    assert!(s.routing.shortcuts.cheapest.is_none());
}
```

Run: `cargo test shortcuts_parse shortcuts_default`
Expected: PASS (after adding the struct).

### 4b — Router

- [ ] **Step 3: Resolve shortcuts in engine.rs**

In `src/router/engine.rs`, at the top of `RequestRouter::resolve`, before the alias resolution loop:

```rust
pub fn resolve(&self, requested_model: &str) -> (String, String) {
    // Shortcut resolution — must come first so `:fastest`/`:cheapest` cannot be
    // shadowed by a user-defined alias with the same name.
    let resolved_input = match requested_model {
        ":fastest" => self.settings.routing.shortcuts.fastest
            .as_deref()
            .unwrap_or(requested_model),
        ":cheapest" => self.settings.routing.shortcuts.cheapest
            .as_deref()
            .unwrap_or(requested_model),
        other => other,
    };
    let mut current = resolved_input.to_string();
    // ... rest of existing alias resolution loop unchanged ...
```

- [ ] **Step 4: Write routing unit tests**

In `src/router/engine.rs`, add a `#[cfg(test)]` block:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::config::{Settings, schema::{RoutingConfig, RoutingShortcutsConfig}};

    fn make_router_with_shortcuts(fastest: Option<&str>, cheapest: Option<&str>) -> RequestRouter {
        let mut settings = Settings::default();
        settings.routing.shortcuts = RoutingShortcutsConfig {
            fastest: fastest.map(str::to_string),
            cheapest: cheapest.map(str::to_string),
        };
        RequestRouter::new(Arc::new(settings))
    }

    #[test]
    fn fastest_resolves_to_configured_model() {
        let r = make_router_with_shortcuts(Some("anthropic/claude-haiku-4-5"), None);
        let (provider, model) = r.resolve(":fastest");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-haiku-4-5");
    }

    #[test]
    fn cheapest_resolves_to_configured_model() {
        let r = make_router_with_shortcuts(None, Some("deepseek/deepseek-chat"));
        let (_, model) = r.resolve(":cheapest");
        assert_eq!(model, "deepseek-chat");
    }

    #[test]
    fn shortcut_not_configured_falls_through_to_default() {
        let r = make_router_with_shortcuts(None, None);
        // ":fastest" with no config resolves to default provider/model
        let (provider, _) = r.resolve(":fastest");
        assert_eq!(provider, "openai");  // default
    }

    #[test]
    fn normal_model_unaffected() {
        let r = make_router_with_shortcuts(Some("x"), Some("y"));
        let (provider, model) = r.resolve("anthropic/claude-opus-4-5");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-5");
    }
}
```

Run: `cargo test router::engine::tests`
Expected: all 4 pass.

### 4c — Admin UI (display only)

- [ ] **Step 5: Show shortcuts on the Models page**

In `templates/admin/models.html`, add a "Routing Shortcuts" card above or below the model list. The shortcuts come from config (not DB), so pass them from the handler:

In `src/api/admin/models.rs`, in the `get_models` handler, add to the template context:

```rust
let shortcuts_fastest = state.settings.routing.shortcuts.fastest.clone();
let shortcuts_cheapest = state.settings.routing.shortcuts.cheapest.clone();
let ctx = minijinja::context! {
    models,
    shortcuts_fastest,
    shortcuts_cheapest,
    // ... other existing context fields ...
};
```

In `templates/admin/models.html`:

```html
<section class="card">
  <h2>Routing Shortcuts</h2>
  <p>Use <code>:fastest</code> or <code>:cheapest</code> as the model name in any API request. Configure targets in <code>config.toml</code> under <code>[routing.shortcuts]</code>.</p>
  <table>
    <thead><tr><th>Keyword</th><th>Resolves To</th></tr></thead>
    <tbody>
      <tr>
        <td><code>:fastest</code></td>
        <td>{% if shortcuts_fastest %}<code>{{ shortcuts_fastest }}</code>{% else %}<em>not configured — falls back to default model</em>{% endif %}</td>
      </tr>
      <tr>
        <td><code>:cheapest</code></td>
        <td>{% if shortcuts_cheapest %}<code>{{ shortcuts_cheapest }}</code>{% else %}<em>not configured — falls back to default model</em>{% endif %}</td>
      </tr>
    </tbody>
  </table>
</section>
```

- [ ] **Step 6: Build and run all tests**

```bash
cargo build
cargo test
```

Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/config/schema.rs src/router/engine.rs templates/admin/models.html src/api/admin/models.rs
git commit -m "feat: add :fastest and :cheapest routing shortcuts configurable via [routing.shortcuts]"
```

---

## Task 5: README + Docs Update

- [ ] **Step 1: Update README Highlights**

Replace the existing Highlights bullet list with:

```markdown
- **Drop-in OpenAI compatibility** — any SDK that speaks `POST /v1/chat/completions` works without modification; native Anthropic `/v1/messages` pass-through also supported
- **Multi-provider routing** — route to OpenAI, Anthropic, Google Gemini, Azure OpenAI, AWS Bedrock, Ollama, or any OpenAI-compatible endpoint including DeepSeek, Qwen, and Doubao; switch providers by changing one config line
- **Routing shortcuts** — use `:fastest` or `:cheapest` as the model name to route to your configured fastest or cheapest model without changing client code
- **Multi-scope budget enforcement** — set daily, weekly, monthly, or fixed date-range limits at the global, project, user, group, and per-key level; any limit hit blocks the request before it reaches the upstream
- **Webhook callbacks** — register outbound webhooks (via admin UI or CLI) that fire after each completion; wire Datadog, Slack, or any HTTP endpoint; takes effect on next restart
- **Prompt logging control** — set `X-No-Log: true` on any request to skip prompt history storage while preserving cost tracking for budget enforcement
- **Admin dashboard** — web UI at `/admin` with usage stats, audit log, and full management pages for users, API keys, groups, budgets, models, and webhooks
- **Declarative policy engine** — TOML-configured rules that match users by project, group, or ID and enforce model allow-lists and budgets without touching the database
- **Content guardrails** — pluggable safety layer runs OpenAI moderation (or a custom HTTP endpoint) on requests and responses; configurable fail-open/fail-closed
- **MCP server registry** — register and discover Model Context Protocol servers via REST; semantic search ranks results by relevance to a query
- **SSO / OIDC** — admin users can authenticate via Google, Okta, Auth0, or any OIDC provider using authorization code flow with PKCE; new admins are auto-provisioned from email allow-lists
- **Hook system** — run shell scripts at lifecycle events and in the request pipeline; grant capabilities per-user via `hook_permissions`
- **Feature-flagged optional components** — `--features postgres` for Postgres backend, `--features otel` for full OpenTelemetry observability (traces, metrics, logs via OTLP)
- **Single static binary** — SQLite bundled, no runtime dependencies; ships as a distroless Docker image
```

- [ ] **Step 2: Update API Endpoints table in README**

Add to the endpoints table:

```markdown
| `GET /admin/webhooks` | Webhook management page |
| `GET /admin/api/webhooks` | List webhook backends (admin JWT required) |
| `POST /admin/api/webhooks` | Create webhook backend (superadmin JWT required) |
| `DELETE /admin/api/webhooks/:id` | Delete webhook backend (superadmin JWT required) |
```

- [ ] **Step 3: Update CLI Commands section in README**

Add:

```markdown
### Webhook Management

```bash
modelrouter webhook list
modelrouter webhook add --name datadog --url https://... --events completion --secret-header-name DD-API-KEY --secret-header-value <key>
modelrouter webhook delete --id 1
modelrouter webhook enable --id 2
modelrouter webhook disable --id 2
```

### Routing Shortcuts

Add to `~/.modelrouter/config.toml`:

```toml
[routing.shortcuts]
fastest  = "anthropic/claude-haiku-4-5"
cheapest = "deepseek/deepseek-chat"
```

Then use `:fastest` or `:cheapest` as the model name in any request:

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer mr-yourkey" \
  -d '{"model":":cheapest","messages":[{"role":"user","content":"Hello"}]}'
```

### Prompt Logging Opt-Out

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer mr-yourkey" \
  -H "X-No-Log: true" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Confidential prompt"}]}'
```

Cost is still tracked for budget enforcement. Only the prompt history entry and outbound callbacks are skipped.
```

- [ ] **Step 4: Update CLAUDE.md API Endpoints table**

The `CLAUDE.md` at the project root lists API endpoints. Add:
```
| `GET/POST /admin/webhooks` | Webhook management (dashboard) |
| `GET/POST/DELETE /admin/api/webhooks` | Webhook CRUD (REST) |
```

- [ ] **Step 5: Commit docs**

```bash
git add README.md CLAUDE.md
git commit -m "docs: update README for logging opt-out, webhook callbacks, Chinese providers, and routing shortcuts"
```

---

## Task 6: Update Competitor Comparison Doc

- [ ] **Step 1: Update the gaps table**

In `docs/plans/competitor-comparison.md`, remove these rows from "What Competitors Have That This Project Lacks":

- "Broadcast / webhook observability push" — this project now has it
- "Zero Data Retention per-request (`zdr: true`)" — this project now has `X-No-Log: true`
- "Domestic Chinese model coverage" — now documented as supported via `openai_compat`

And update the "What This Project Has" table to add:
- "DB-managed webhook callbacks with admin UI + CLI"
- "`X-No-Log: true` per-request prompt skip"
- "`:fastest`/`:cheapest` routing shortcuts"
- "Chinese model provider support (DeepSeek, Qwen, Doubao)"

- [ ] **Step 2: Commit**

```bash
git add docs/plans/competitor-comparison.md
git commit -m "docs: update competitor comparison to reflect Phase 21 features"
```

---

## Appendix A: Session Stickiness — Plan (Do Not Implement Now)

**Goal:** Route multi-turn conversations from the same `session_id` to the same (provider, model) for the duration of the session.

**Files to create/modify when implementing:**
- Create: `src/router/session_affinity.rs` — `SessionAffinityMap`: `Arc<DashMap<String, (String, String)>>` mapping `session_id → (provider, model)`; entries have a TTL (30 min); checked before load balancer, after model resolution
- Modify: `src/api/routes/completions.rs` — read `body["session_id"]` (already partially handled for rate limiting); before load balancer, check `session_affinity.get(session_id)`; after provider selection, store `session_affinity.insert(session_id, (provider, model))`
- Modify: `src/api/app.rs` — add `session_affinity: Arc<SessionAffinityMap>` to `AppState`
- Modify: `README.md` — document `session_id` field

**TTL strategy:** On each request with a matching `session_id`, refresh the TTL. Stale sessions (no requests for >30 min) are evicted. Use `tokio::time` or a background sweeper.

**Admin UI:** Show active session count on Overview dashboard (query `DashMap::len()`).

---

## Appendix B: Not Diamond Hook Example

Create `docs/hooks/not-diamond-router.md` with the following content:

---

### Using Not Diamond for ML-Driven Auto-Routing via a Pipeline Hook

Not Diamond provides a client-side routing API that recommends which LLM to use for a given prompt. You can integrate it as a `request.pre` pipeline hook — modelrouter calls your script before forwarding to the upstream, and your script can override the `model` field.

**1. Install the Not Diamond Python SDK:**

```bash
pip install notdiamond
```

**2. Create the hook script at `/usr/local/bin/mr-notdiamond-hook.py`:**

```python
#!/usr/bin/env python3
"""
Modelrouter request.pre hook — routes to Not Diamond's recommended model.

Reads a JSON request body from stdin, calls Not Diamond's routing API,
and writes the (possibly mutated) request body to stdout.
"""
import json
import os
import sys

def main():
    body = json.load(sys.stdin)
    
    # Extract messages for Not Diamond routing
    messages = body.get("messages", [])
    if not messages:
        json.dump(body, sys.stdout)
        return

    try:
        from notdiamond import NotDiamond
        client = NotDiamond(api_key=os.environ["NOTDIAMOND_API_KEY"])
        
        # Candidate models — must match modelrouter provider/model format
        # Map Not Diamond model IDs to modelrouter provider/model strings
        MODEL_MAP = {
            "openai/gpt-4o":                  "openai/gpt-4o",
            "openai/gpt-4o-mini":             "openai/gpt-4o-mini",
            "anthropic/claude-opus-4-5":      "anthropic/claude-opus-4-5",
            "anthropic/claude-haiku-4-5":     "anthropic/claude-haiku-4-5",
            "google/gemini-1.5-pro":          "google/gemini-1.5-pro",
            "deepseek/deepseek-chat":         "deepseek/deepseek-chat",
        }
        
        # Build Not Diamond message format
        nd_messages = [
            {"role": m["role"], "content": m.get("content", "")}
            for m in messages
            if isinstance(m.get("content"), str)
        ]
        
        result, session_id, provider = client.chat.completions.model_select(
            messages=nd_messages,
            model=list(MODEL_MAP.keys()),
        )
        
        # Override the model field with the Not Diamond recommendation
        recommended = MODEL_MAP.get(str(provider), None)
        if recommended:
            body["model"] = recommended
            
    except Exception as e:
        # Fail-open: log and pass through original request
        import sys
        print(f"not-diamond hook error: {e}", file=sys.stderr)
    
    json.dump(body, sys.stdout)

if __name__ == "__main__":
    main()
```

**3. Make it executable:**

```bash
chmod +x /usr/local/bin/mr-notdiamond-hook.py
```

**4. Add the hook to your modelrouter config:**

```toml
[[hooks.pipeline]]
name        = "not-diamond-router"
event       = "request.pre"
exec        = "/usr/local/bin/mr-notdiamond-hook.py"
capabilities = ["mutate_request"]
timeout_secs = 3
fail_open   = true   # if Not Diamond is down, pass through original request
```

**5. Grant the hook capability to your users:**

```sql
INSERT INTO hook_permissions (user_id, hook_name, capability)
VALUES (<user_id>, 'not-diamond-router', 'mutate_request');
```

Or for all users, insert for each user in your `users` table.

**How it works:** modelrouter serializes the request body to JSON and pipes it to stdin of your script. Your script calls Not Diamond's `/v1/model-select` endpoint (locally, ~100–200ms), overrides the `model` field in the JSON, and writes the mutated body to stdout. modelrouter then routes the request to the Not Diamond-recommended provider.

**Cost:** Not Diamond charges per routing call, not per token — typically far less than the token savings from optimal model selection. Set `fail_open = true` so a Not Diamond outage doesn't block your traffic.

---

*End of plan.*
