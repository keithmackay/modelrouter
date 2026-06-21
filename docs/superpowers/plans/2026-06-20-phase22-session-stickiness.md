# Phase 22: Session Stickiness with Model-Change Override

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route multi-turn conversations with the same `session_id` to the same upstream provider, while automatically re-pinning when the caller explicitly switches models mid-session.

**Architecture:** An in-memory `DashMap<String, SessionPin>` maps `session_id → (provider, model, last_seen_secs)`. On each request the router checks the pin before the load balancer. A model change (detected by comparing the resolved canonical model after alias/shortcut resolution) clears the old pin and writes a new one. A background task sweeps expired entries every 5 minutes. `X-Session-Lb: true` header lets callers opt out of pin lookup for a single request without clearing the pin.

**Tech Stack:** Rust, `dashmap` (already a dep), `tokio::time`, axum header extraction. No DB changes — affinity is ephemeral and intentionally lost on restart.

---

## Routing Logic (full decision tree)

```
Request arrives with session_id = "abc", model = "opus"
│
├─ Resolve requested model through shortcuts + aliases
│   → canonical = "claude-opus-4-5", provider = "anthropic"
│
├─ X-Session-Lb: true?
│   YES → skip pin lookup; route normally (LB/default); update pin with result
│   NO  → continue
│
├─ Pin exists for "abc"?
│   NO  → route normally; store pin (provider="anthropic", model="claude-opus-4-5")
│   YES → pin is ("anthropic", "claude-opus-4-5")
│         same provider? YES, same model? YES → use pin, refresh TTL
│         model differs but same provider? → re-pin provider, use new model, same provider
│         provider differs? → clear old pin, route normally, store new pin
│
└─ Response → done
```

**Key rules:**
- Comparison is on **resolved canonical model** (after alias/shortcut resolution), not the raw request string
- Same provider + different model (e.g. haiku→sonnet both on Anthropic) → keep provider pin, use new model
- Different provider → clear pin, let LB/default pick, store new pin with whatever was selected
- `X-Session-Lb: true` skips the lookup but still **writes/updates** the pin with whatever the LB chose (so the next normal request gets a fresh pin)
- Pin TTL is 30 minutes; refreshed on every request that reads or writes it
- TTL is intentionally not configurable for now — keep it simple

---

## Files Created or Modified

| File | Action | Purpose |
|---|---|---|
| `src/router/session_affinity.rs` | Create | `SessionAffinityMap` struct + all logic |
| `src/router/mod.rs` | Modify | `pub mod session_affinity` |
| `src/api/app.rs` | Modify | Add `session_affinity` to `AppState`; construct in builder |
| `src/api/routes/completions.rs` | Modify | Stickiness check + pin write in hot path |
| `README.md` | Modify | Session stickiness section + For Developers subsection |

---

## Task 1: `SessionAffinityMap`

**Files:**
- Create: `src/router/session_affinity.rs`
- Modify: `src/router/mod.rs`

- [ ] **Step 1: Write the module**

Create `src/router/session_affinity.rs`:

```rust
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct SessionPin {
    pub provider: String,
    pub model: String,
    last_seen: u64, // unix seconds
}

pub struct SessionAffinityMap {
    ttl_secs: u64,
    map: DashMap<String, SessionPin>,
    /// Approximate entry count for the overview dashboard.
    count: AtomicU64,
}

impl SessionAffinityMap {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            map: DashMap::new(),
            count: AtomicU64::new(0),
        }
    }

    /// Look up an existing, non-expired pin. Refreshes TTL on hit.
    pub fn get(&self, session_id: &str) -> Option<SessionPin> {
        let mut entry = self.map.get_mut(session_id)?;
        let now = now_secs();
        if now.saturating_sub(entry.last_seen) > self.ttl_secs {
            drop(entry);
            self.map.remove(session_id);
            self.count.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        entry.last_seen = now;
        Some(entry.clone())
    }

    /// Store or overwrite a pin.
    pub fn set(&self, session_id: &str, provider: &str, model: &str) {
        let is_new = !self.map.contains_key(session_id);
        self.map.insert(session_id.to_string(), SessionPin {
            provider: provider.to_string(),
            model: model.to_string(),
            last_seen: now_secs(),
        });
        if is_new {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Remove a pin explicitly.
    pub fn remove(&self, session_id: &str) {
        if self.map.remove(session_id).is_some() {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Approximate number of live sessions (may include expired entries not yet swept).
    pub fn len(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Evict all entries older than TTL. Call periodically from a background task.
    pub fn evict_expired(&self) {
        let now = now_secs();
        let ttl = self.ttl_secs;
        let mut removed = 0u64;
        self.map.retain(|_, pin| {
            let keep = now.saturating_sub(pin.last_seen) <= ttl;
            if !keep { removed += 1; }
            keep
        });
        if removed > 0 {
            self.count.fetch_sub(removed, Ordering::Relaxed);
        }
    }
}

/// Determine how to route given a session pin and the newly-resolved (provider, model).
/// Returns the (provider, model) to actually use, and whether the pin should be updated.
pub fn resolve_with_pin(
    pin: Option<&SessionPin>,
    resolved_provider: &str,
    resolved_model: &str,
) -> (String, String, bool) {
    match pin {
        None => {
            // No pin — use resolved, store new pin
            (resolved_provider.to_string(), resolved_model.to_string(), true)
        }
        Some(p) if p.provider == resolved_provider && p.model == resolved_model => {
            // Exact match — use pin, just refresh TTL
            (p.provider.clone(), p.model.clone(), true)
        }
        Some(p) if p.provider == resolved_provider => {
            // Same provider, different model — keep provider pin, use new model, update pin
            tracing::debug!(
                session_provider = p.provider.as_str(),
                old_model = p.model.as_str(),
                new_model = resolved_model,
                "session model change within same provider — re-pinning model"
            );
            (p.provider.clone(), resolved_model.to_string(), true)
        }
        Some(p) => {
            // Different provider — caller explicitly changed providers; clear pin, use resolved
            tracing::debug!(
                old_provider = p.provider.as_str(),
                new_provider = resolved_provider,
                "session provider change — clearing pin"
            );
            (resolved_provider.to_string(), resolved_model.to_string(), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pin_stores_and_returns_resolved() {
        let (p, m, update) = resolve_with_pin(None, "anthropic", "claude-opus-4-5");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-5");
        assert!(update);
    }

    #[test]
    fn exact_match_uses_pin() {
        let pin = SessionPin { provider: "anthropic".into(), model: "claude-opus-4-5".into(), last_seen: 0 };
        let (p, m, update) = resolve_with_pin(Some(&pin), "anthropic", "claude-opus-4-5");
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-opus-4-5");
        assert!(update);
    }

    #[test]
    fn same_provider_different_model_keeps_provider() {
        let pin = SessionPin { provider: "anthropic".into(), model: "claude-haiku-4-5".into(), last_seen: 0 };
        let (p, m, update) = resolve_with_pin(Some(&pin), "anthropic", "claude-opus-4-5");
        assert_eq!(p, "anthropic"); // provider preserved
        assert_eq!(m, "claude-opus-4-5"); // new model used
        assert!(update);
    }

    #[test]
    fn different_provider_clears_pin() {
        let pin = SessionPin { provider: "anthropic".into(), model: "claude-opus-4-5".into(), last_seen: 0 };
        let (p, m, update) = resolve_with_pin(Some(&pin), "openai", "gpt-4o");
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");
        assert!(update);
    }

    #[test]
    fn map_set_and_get() {
        let map = SessionAffinityMap::new(1800);
        map.set("sess1", "anthropic", "claude-opus-4-5");
        let pin = map.get("sess1").unwrap();
        assert_eq!(pin.provider, "anthropic");
        assert_eq!(pin.model, "claude-opus-4-5");
    }

    #[test]
    fn map_expired_entry_returns_none() {
        let map = SessionAffinityMap::new(0); // TTL = 0 → always expired
        map.set("sess1", "anthropic", "claude-opus-4-5");
        // Force last_seen to be old
        if let Some(mut e) = map.map.get_mut("sess1") {
            e.last_seen = 0;
        }
        assert!(map.get("sess1").is_none());
    }

    #[test]
    fn evict_expired_removes_old_entries() {
        let map = SessionAffinityMap::new(1800);
        map.set("old", "openai", "gpt-4o");
        map.set("new", "anthropic", "claude-haiku-4-5");
        // Force "old" to be expired
        if let Some(mut e) = map.map.get_mut("old") {
            e.last_seen = 0;
        }
        map.evict_expired();
        assert!(map.get("old").is_none());
        assert!(map.get("new").is_some());
    }
}
```

- [ ] **Step 2: Add to router mod**

In `src/router/mod.rs`, add:
```rust
pub mod session_affinity;
```

- [ ] **Step 3: Run tests**

```bash
cargo test session_affinity
```
Expected: 7 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/router/session_affinity.rs src/router/mod.rs
git commit -m "feat: add SessionAffinityMap with TTL, model-change override logic, and eviction"
```

---

## Task 2: Wire into AppState + background sweeper

**Files:**
- Modify: `src/api/app.rs`

- [ ] **Step 1: Add to AppState**

In `src/api/app.rs`, add to `AppState`:

```rust
pub session_affinity: Arc<crate::router::session_affinity::SessionAffinityMap>,
```

- [ ] **Step 2: Construct in builder**

Wherever `AppState` is constructed (in `src/cli/mod.rs` or `src/api/app.rs`), add:

```rust
let session_affinity = Arc::new(
    crate::router::session_affinity::SessionAffinityMap::new(30 * 60) // 30-minute TTL
);
```

And include it in the `AppState { ... }` struct literal.

- [ ] **Step 3: Spawn background sweeper**

After constructing `AppState` (before returning from the builder or in the serve function), spawn a background task:

```rust
{
    let affinity = Arc::clone(&session_affinity);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5 * 60));
        loop {
            interval.tick().await;
            affinity.evict_expired();
            tracing::debug!(
                active_sessions = affinity.len(),
                "session affinity sweep complete"
            );
        }
    });
}
```

- [ ] **Step 4: Build**

```bash
cargo build
```

Expected: clean compile.

- [ ] **Step 5: Commit**

```bash
git add src/api/app.rs src/cli/mod.rs
git commit -m "feat: wire SessionAffinityMap into AppState with 5-minute background sweeper"
```

---

## Task 3: Stickiness in the completions hot path

**Files:**
- Modify: `src/api/routes/completions.rs`

Read the file in full before editing. The insertion point is after `canonical_model` / `provider_name` are resolved (after the load balancer block, around line 183–195) and before the streaming/non-streaming split.

- [ ] **Step 1: Add X-Session-Lb helper**

At the bottom of `completions.rs` alongside `should_skip_logging`, add:

```rust
/// Returns true if the caller opted out of session stickiness for this request.
pub fn should_skip_affinity(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-session-lb")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
```

- [ ] **Step 2: Insert stickiness block after provider/model resolution**

After the `let (provider_name, canonical_model) = ...` block that resolves via load balancer or router, add:

```rust
// Session stickiness — pin this session to the resolved provider when session_id is present
let skip_affinity = should_skip_affinity(&headers);
let (provider_name, canonical_model) = if let Some(session_id) = body["session_id"].as_str() {
    use crate::router::session_affinity::resolve_with_pin;

    let pin = if skip_affinity {
        None // skip lookup; still write pin below with whatever we resolved
    } else {
        state.session_affinity.get(session_id)
    };

    let (pinned_provider, pinned_model, should_update) =
        resolve_with_pin(pin.as_ref(), &provider_name, &canonical_model);

    if should_update {
        state.session_affinity.set(session_id, &pinned_provider, &pinned_model);
    }

    (pinned_provider, pinned_model)
} else {
    (provider_name, canonical_model)
};
```

Note: the `(provider_name, canonical_model)` binding from the load balancer block becomes the input to this block. The shadowing is intentional.

- [ ] **Step 3: Add unit tests**

Add to the `#[cfg(test)]` block in `completions.rs`:

```rust
#[test]
fn skip_affinity_header_detected() {
    let mut h = axum::http::HeaderMap::new();
    h.insert("x-session-lb", "true".parse().unwrap());
    assert!(should_skip_affinity(&h));
}

#[test]
fn skip_affinity_absent_is_false() {
    assert!(!should_skip_affinity(&axum::http::HeaderMap::new()));
}
```

- [ ] **Step 4: Build and test**

```bash
cargo build
cargo test session_affinity
cargo test skip_affinity
cargo test --lib
```

All must pass.

- [ ] **Step 5: Commit**

```bash
git add src/api/routes/completions.rs
git commit -m "feat: apply session stickiness in completions hot path with X-Session-Lb override"
```

---

## Task 4: README documentation

**Files:**
- Modify: `README.md`

Find the `## Configuration` section or the routing section. Add a new top-level section `## Session Stickiness` (or place it logically under routing). Include both a general behavior description and a "For Developers" subsection.

Content:

````markdown
## Session Stickiness

When a request includes a `session_id` field, modelrouter pins that session to the upstream provider it selects on the first request. Every subsequent request with the same `session_id` goes to the same provider — even if a load balancer pool is configured that would otherwise distribute traffic.

```json
{
  "model": "claude-opus-4-5",
  "session_id": "user-42-conv-891",
  "messages": [...]
}
```

Pins expire after 30 minutes of inactivity and are stored in memory — they are not persisted across server restarts.

### Why stickiness matters

Many providers offer prompt caching: if the same long prefix (system prompt, document, conversation history) appears in consecutive requests, the provider reuses its cached computation and charges a fraction of the normal input rate. Caches are local to a specific provider endpoint. Routing turn 3 of a conversation to a different provider than turns 1 and 2 produces a cache miss and charges full price.

Stickiness ensures that a session's accumulated context always lands on the same provider, keeping the cache warm.

### Model changes mid-session

If a request in a pinned session specifies a different model than the one in use, modelrouter updates the pin rather than ignoring the change:

| Change | Behaviour |
|---|---|
| Same model, same provider | Use pin, refresh TTL |
| Different model, **same provider** | Keep provider pin, use new model, update pin |
| Different model, **different provider** | Clear old pin, route normally, store new pin |

This means switching between two Anthropic models mid-session keeps traffic on Anthropic (preserving the provider relationship and any cached prefix), while switching from Claude to GPT-4o routes to OpenAI and starts a fresh pin.

Model comparisons use the **resolved** canonical model after alias and shortcut expansion — switching from `"opus"` to `"anthropic/claude-opus-4-5"` (where `opus` is an alias) is recognised as the same model and does not update the pin.

### For Developers

**When to include `session_id`:**
- Multi-turn conversations where context accumulates across turns
- Any use case with a long system prompt or document that benefits from provider-side caching
- Agentic loops where the same task context is reused across many tool calls

**When to omit `session_id`:**
- Single-shot requests with no shared context
- Batch jobs where each request is independent and load distribution matters more than caching
- High-throughput pipelines where you want the load balancer to spread traffic freely

**Opting out of stickiness for one request:**

If you have a session open but a specific request is stateless and you want the load balancer to choose freely, set `X-Session-Lb: true`:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer mr-yourkey" \
  -H "X-Session-Lb: true" \
  -d '{
    "model": "gpt-4o",
    "session_id": "user-42-conv-891",
    "messages": [{"role": "user", "content": "What is 2+2?"}]
  }'
```

The load balancer picks freely, and the result becomes the new pin for the session going forward.

**Synthetic session IDs for opaque clients:**

Tools like Claude Code or Codex do not include `session_id` in their requests. To enable stickiness for these clients, use a `request.pre` pipeline hook to inject a synthetic `session_id` derived from the API key and a rolling time window:

```python
#!/usr/bin/env python3
import json, sys, time, hashlib

body = json.load(sys.stdin)
# 30-minute bucket keyed by API key tag (injected by modelrouter as x-api-key-id)
bucket = int(time.time()) // 1800
api_key_id = body.get("_mr_api_key_id", "default")  # set by pre-request hook context
body.setdefault("session_id", hashlib.sha256(f"{api_key_id}:{bucket}".encode()).hexdigest()[:16])
json.dump(body, sys.stdout)
```

Configure the hook in `config.toml`:

```toml
[[hooks.pipeline]]
name         = "synthetic-session-id"
event        = "request.pre"
exec         = "/usr/local/bin/mr-synthetic-session.py"
capabilities = ["mutate_request"]
timeout_secs = 1
fail_open    = true
```
````

- [ ] **Step 1: Add the section to README**

Find the correct location (after the Routing section, before or after Configuration). Insert the full block above.

- [ ] **Step 2: Build (docs-only change, just verify no broken build)**

```bash
cargo build
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add Session Stickiness section with model-change override and For Developers guide"
```
