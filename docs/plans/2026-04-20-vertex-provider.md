# GCP Vertex AI Provider Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a feature-gated `vertex` provider to modelrouter that proxies OpenAI-shaped requests to GCP Vertex AI for both Gemini (publisher `google`) and Claude on Vertex (publisher `anthropic`).

**Architecture:** New module `src/providers/vertex/` mirrors the `bedrock` pattern — compiled only under `--features vertex`. A `VertexAdapter` implements `ProviderAdapter`, parses the model identifier (`google/gemini-2.5-pro` or `anthropic/claude-sonnet-4-6@20250514`) to select a publisher, calls the native Vertex endpoint with a Google OAuth2 Bearer token (obtained via `google-cloud-auth`, cached + auto-refreshed), and translates both request and response shapes to/from OpenAI format. Streaming re-emits Vertex SSE as `chat.completion.chunk` and synthesises a trailing `data: [DONE]\n\n`.

**Tech Stack:** Rust 1.91, axum, reqwest, sqlx, async-trait, futures, serde_json. Adds `google-cloud-auth` (official Google crate) gated behind `vertex` feature.

**Non-goals for this plan:** Llama-on-Vertex (can be added later), embeddings, image models, TPU/custom endpoints, workload identity federation (ADC + SA JSON only for MVP).

---

## Ground rules

- **TDD.** Every task writes failing tests before implementation.
- **Pure translation functions are `pub`** so unit tests can import them (mirrors `bedrock.rs:26`).
- **Keep the adapter `pub`-hidden**; only expose what tests need.
- **One commit per task** unless a task note says otherwise.
- **All code behind `#[cfg(feature = "vertex")]`.** Do not leak `google-cloud-auth` imports into default builds.
- **Never hit real Google OAuth in tests.** Auth is behind a `TokenProvider` trait; tests inject a fake.

---

## Task 0: Worktree + branch

**Step 1:** Create a worktree for the feature

```bash
git -C /Users/Michael.Stricklen/dev/modelrouter worktree add ../modelrouter-vertex -b feat/vertex-provider
cd ../modelrouter-vertex
```

**Step 2:** Verify clean state

```bash
cargo build --release 2>&1 | tail -5
```

Expected: clean build, no warnings introduced by us. If it already fails on `main`, stop and report before proceeding.

---

## Task 1: Cargo feature + dependency

**Files:**
- Modify: `Cargo.toml`

**Step 1:** Add the `vertex` feature and dependency. Look up the latest `google-cloud-auth` version.

```bash
cargo search google-cloud-auth --limit 3
```

**Step 2:** Edit `Cargo.toml`:

In `[features]`:
```toml
vertex = ["dep:google-cloud-auth"]
```

In `[dependencies]`, after the bedrock block (`src line ~71`):
```toml
# ── GCP Vertex (--features vertex) ──────────────────────────────────────
google-cloud-auth = { version = "<pin to latest from cargo search>", optional = true }
```

**Step 3:** Verify default build unaffected

```bash
cargo build --release 2>&1 | tail -5
```
Expected: success.

**Step 4:** Verify vertex feature compiles (will build the dep even though no Rust code uses it yet)

```bash
cargo build --features vertex 2>&1 | tail -5
```
Expected: success.

**Step 5:** Commit

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat(vertex): add vertex feature flag and google-cloud-auth dep"
```

---

## Task 2: Extend `ProviderConfig` with `project` and `credentials_path`

**Files:**
- Modify: `src/config/schema.rs:307-321`
- Modify: `tests/test_azure.rs:10-12` and other callers (any `ProviderConfig { ... }` struct literal needs the new fields)
- Test: `tests/test_bedrock.rs:4-13` also constructs `ProviderConfig`

**Step 1:** Grep for all callers so we know what will break

```bash
# Use Grep tool (not rg) — from the worktree root
```
Search `ProviderConfig \{` in `src/` and `tests/`. Note every file that constructs one.

**Step 2:** Write a failing test at `tests/test_vertex.rs` (create file):

```rust
use modelrouter::config::schema::ProviderConfig;

#[test]
fn provider_config_has_project_and_credentials_path() {
    let config = ProviderConfig {
        api_key: String::new(),
        api_base: None,
        timeout_secs: 60,
        api_version: None,
        region: Some("us-east5".into()),
        project: Some("my-proj".into()),
        credentials_path: Some("/secrets/sa.json".into()),
    };
    assert_eq!(config.project.as_deref(), Some("my-proj"));
    assert_eq!(config.credentials_path.as_deref(), Some("/secrets/sa.json"));
}
```

**Step 3:** Run the test — expect compile failure

```bash
cargo test --test test_vertex 2>&1 | tail -10
```
Expected: `error[E0063]: missing field` or `no field 'project'`.

**Step 4:** Edit `src/config/schema.rs:307` to add the fields:

```rust
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    /// GCP project ID. Used only by the Vertex adapter.
    #[serde(default)]
    pub project: Option<String>,
    /// Path to GCP service-account JSON. If None, uses Application Default Credentials.
    /// Used only by the Vertex adapter.
    #[serde(default)]
    pub credentials_path: Option<String>,
}
```

**Step 5:** Update every `ProviderConfig { ... }` literal in tests and source found in Step 1. Add `project: None, credentials_path: None,` to each.

**Step 6:** Run full test suite to verify nothing else broke

```bash
cargo test 2>&1 | tail -20
```
Expected: all green (same count as before plus the new vertex test).

**Step 7:** Commit

```bash
git add src/config/schema.rs tests/
git commit -m "feat(vertex): add project + credentials_path to ProviderConfig"
```

---

## Task 3: Publisher dispatch

**Files:**
- Create: `src/providers/vertex/mod.rs`
- Create: `src/providers/vertex/dispatch.rs`
- Modify: `src/providers/mod.rs:1-11`
- Test: `tests/test_vertex.rs`

**Step 1:** Add to `tests/test_vertex.rs`:

```rust
#[cfg(feature = "vertex")]
mod dispatch_tests {
    use modelrouter::providers::vertex::dispatch::{parse_model_id, Publisher};

    #[test]
    fn gemini_prefix_parses_to_google() {
        let (pub_, id) = parse_model_id("google/gemini-2.5-pro").unwrap();
        assert_eq!(pub_, Publisher::Google);
        assert_eq!(id, "gemini-2.5-pro");
    }

    #[test]
    fn anthropic_prefix_with_version_parses() {
        let (pub_, id) = parse_model_id("anthropic/claude-sonnet-4-6@20250514").unwrap();
        assert_eq!(pub_, Publisher::Anthropic);
        assert_eq!(id, "claude-sonnet-4-6@20250514");
    }

    #[test]
    fn bare_gemini_name_defaults_to_google() {
        let (pub_, id) = parse_model_id("gemini-2.5-flash").unwrap();
        assert_eq!(pub_, Publisher::Google);
        assert_eq!(id, "gemini-2.5-flash");
    }

    #[test]
    fn bare_claude_name_defaults_to_anthropic() {
        let (pub_, id) = parse_model_id("claude-opus-4-5@20250101").unwrap();
        assert_eq!(pub_, Publisher::Anthropic);
        assert_eq!(id, "claude-opus-4-5@20250101");
    }

    #[test]
    fn unknown_prefix_errors() {
        let err = parse_model_id("cohere/command-r").unwrap_err().to_string();
        assert!(err.contains("Unsupported Vertex publisher"), "got: {err}");
    }
}
```

**Step 2:** Run — expect failure (module doesn't exist)

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -10
```

**Step 3:** Create `src/providers/vertex/mod.rs`:

```rust
//! GCP Vertex AI provider (--features vertex).
pub mod dispatch;
```

**Step 4:** Create `src/providers/vertex/dispatch.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publisher {
    Google,
    Anthropic,
}

/// Parse a model identifier into (publisher, bare_model_id).
///
/// Accepts either a publisher-prefixed id (`google/gemini-2.5-pro`,
/// `anthropic/claude-sonnet-4-6@20250514`) or a bare id whose name prefix
/// disambiguates the publisher (`gemini-*` → Google, `claude-*` → Anthropic).
pub fn parse_model_id(model: &str) -> anyhow::Result<(Publisher, String)> {
    if let Some((prefix, rest)) = model.split_once('/') {
        let pub_ = match prefix {
            "google" => Publisher::Google,
            "anthropic" => Publisher::Anthropic,
            other => anyhow::bail!("Unsupported Vertex publisher '{}'", other),
        };
        return Ok((pub_, rest.to_string()));
    }
    if model.starts_with("gemini-") {
        return Ok((Publisher::Google, model.to_string()));
    }
    if model.starts_with("claude-") {
        return Ok((Publisher::Anthropic, model.to_string()));
    }
    anyhow::bail!("Unsupported Vertex publisher (cannot infer from model id '{}')", model)
}
```

**Step 5:** Add to `src/providers/mod.rs`:

```rust
#[cfg(feature = "vertex")]
pub mod vertex;
```

**Step 6:** Run tests — expect all five to pass

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -10
```

**Step 7:** Commit

```bash
git add src/providers/mod.rs src/providers/vertex/ tests/test_vertex.rs
git commit -m "feat(vertex): publisher dispatch and module scaffolding"
```

---

## Task 4: Gemini translation (request + response + SSE)

**Files:**
- Create: `src/providers/vertex/gemini.rs`
- Modify: `src/providers/vertex/mod.rs` (add `pub mod gemini;`)
- Test: `tests/test_vertex.rs`

**Rationale:** Pure JSON-in/JSON-out translation. No HTTP. Each function is directly unit-testable.

**Step 1:** Add tests to `tests/test_vertex.rs`:

```rust
#[cfg(feature = "vertex")]
mod gemini_tests {
    use modelrouter::providers::adapter::NormalizedRequest;
    use modelrouter::providers::vertex::gemini::{
        translate_request, parse_response, translate_sse_line,
    };
    use serde_json::json;

    fn req(messages: serde_json::Value) -> NormalizedRequest {
        NormalizedRequest {
            model: "gemini-2.5-pro".into(),
            messages: messages.as_array().unwrap().clone(),
            stream: false,
            temperature: Some(0.7),
            max_tokens: Some(1024),
            extra_params: json!({}),
        }
    }

    #[test]
    fn translate_request_extracts_system_instruction() {
        let r = req(json!([
            {"role": "system", "content": "Be helpful."},
            {"role": "user", "content": "Hi"}
        ]));
        let body = translate_request(&r);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be helpful.");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "Hi");
    }

    #[test]
    fn translate_request_maps_assistant_to_model_role() {
        let r = req(json!([
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello!"}
        ]));
        let body = translate_request(&r);
        assert_eq!(body["contents"][1]["role"], "model");
    }

    #[test]
    fn translate_request_emits_generation_config() {
        let r = req(json!([{"role": "user", "content": "Hi"}]));
        let body = translate_request(&r);
        assert_eq!(body["generationConfig"]["temperature"], 0.7);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
    }

    #[test]
    fn parse_response_extracts_text_and_usage() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hi there!"}], "role": "model"},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 4,
                "totalTokenCount": 16
            }
        });
        let cr = parse_response(resp).unwrap();
        assert_eq!(cr.content, "Hi there!");
        assert_eq!(cr.prompt_tokens, 12);
        assert_eq!(cr.completion_tokens, 4);
        assert_eq!(cr.finish_reason, "stop");
    }

    #[test]
    fn parse_response_maps_max_tokens_finish_reason() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": "..."}]},
                "finishReason": "MAX_TOKENS"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
        });
        let cr = parse_response(resp).unwrap();
        assert_eq!(cr.finish_reason, "length");
    }

    #[test]
    fn parse_response_maps_safety_finish_reason() {
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": ""}]},
                "finishReason": "SAFETY"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 0, "totalTokenCount": 1}
        });
        assert_eq!(parse_response(resp).unwrap().finish_reason, "content_filter");
    }

    #[test]
    fn translate_sse_line_emits_openai_chunk() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hi"}]}}]}"#;
        let out = translate_sse_line(line).unwrap();
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains(r#""delta":{"content":"Hi"}"#));
        assert!(out_str.contains(r#""object":"chat.completion.chunk""#));
    }

    #[test]
    fn translate_sse_line_skips_empty_lines() {
        assert!(translate_sse_line("").is_none());
        assert!(translate_sse_line("\n").is_none());
        assert!(translate_sse_line("event: ping").is_none());
    }
}
```

**Step 2:** Run — expect module-not-found failure

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -10
```

**Step 3:** Create `src/providers/vertex/gemini.rs`:

```rust
use bytes::Bytes;
use crate::providers::adapter::{CompletionResult, NormalizedRequest};

/// Translate an OpenAI-shaped request to a Gemini `generateContent` body.
pub fn translate_request(req: &NormalizedRequest) -> serde_json::Value {
    let mut system_parts: Vec<serde_json::Value> = Vec::new();
    let mut contents: Vec<serde_json::Value> = Vec::new();

    for m in &req.messages {
        let role = m["role"].as_str().unwrap_or("");
        let text = m["content"].as_str().unwrap_or("").to_string();
        match role {
            "system" => system_parts.push(serde_json::json!({"text": text})),
            "user" => contents.push(serde_json::json!({
                "role": "user",
                "parts": [{"text": text}]
            })),
            "assistant" => contents.push(serde_json::json!({
                "role": "model",
                "parts": [{"text": text}]
            })),
            _ => {}
        }
    }

    let mut body = serde_json::json!({
        "contents": contents,
    });

    if !system_parts.is_empty() {
        body["systemInstruction"] = serde_json::json!({"parts": system_parts});
    }

    let mut gen_config = serde_json::Map::new();
    if let Some(t) = req.temperature { gen_config.insert("temperature".into(), serde_json::json!(t)); }
    if let Some(m) = req.max_tokens { gen_config.insert("maxOutputTokens".into(), serde_json::json!(m)); }
    if !gen_config.is_empty() {
        body["generationConfig"] = serde_json::Value::Object(gen_config);
    }

    body
}

/// Map a Gemini `finishReason` to OpenAI's.
fn map_finish_reason(r: &str) -> &'static str {
    match r {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content_filter",
        "RECITATION" => "stop",
        _ => "stop",
    }
}

/// Parse a Gemini non-streaming response into the shared CompletionResult.
pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let candidate = v["candidates"].get(0)
        .ok_or_else(|| anyhow::anyhow!("Gemini response has no candidates"))?;
    let content: String = candidate["content"]["parts"].as_array()
        .map(|parts| {
            parts.iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let finish = candidate["finishReason"].as_str().unwrap_or("STOP");
    let usage = &v["usageMetadata"];
    let prompt = usage["promptTokenCount"].as_u64().unwrap_or(0) as u32;
    let completion = usage["candidatesTokenCount"].as_u64().unwrap_or(0) as u32;
    Ok(CompletionResult {
        content,
        prompt_tokens: prompt,
        completion_tokens: completion,
        finish_reason: map_finish_reason(finish).to_string(),
    })
}

/// Translate a single Gemini SSE line to an OpenAI `chat.completion.chunk` line.
/// Returns None for comments, blank lines, or non-data events.
pub fn translate_sse_line(line: &str) -> Option<Bytes> {
    let payload = line.strip_prefix("data: ")?;
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let text = v["candidates"].get(0)?["content"]["parts"].as_array()?
        .iter()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let chunk = serde_json::json!({
        "id": "chatcmpl-vertex-stream",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    });
    Some(Bytes::from(format!("data: {}\n\n", chunk)))
}
```

**Step 4:** Add `pub mod gemini;` to `src/providers/vertex/mod.rs`.

**Step 5:** Run tests — expect all pass

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -15
```

**Step 6:** Commit

```bash
git add src/providers/vertex/ tests/test_vertex.rs
git commit -m "feat(vertex): gemini request/response/SSE translation"
```

---

## Task 5: Claude-on-Vertex translation (request + response + SSE)

**Files:**
- Create: `src/providers/vertex/claude.rs`
- Modify: `src/providers/vertex/mod.rs` (add `pub mod claude;`)
- Test: `tests/test_vertex.rs`

**Key difference from Anthropic direct:** `model` field is **NOT** in the body (it's in the URL), and `anthropic_version: "vertex-2023-10-16"` is required.

**Step 1:** Add tests to `tests/test_vertex.rs`:

```rust
#[cfg(feature = "vertex")]
mod claude_tests {
    use modelrouter::providers::adapter::NormalizedRequest;
    use modelrouter::providers::vertex::claude::{
        translate_request, parse_response, translate_sse_line,
    };
    use serde_json::json;

    fn req(messages: serde_json::Value) -> NormalizedRequest {
        NormalizedRequest {
            model: "claude-sonnet-4-6@20250514".into(),
            messages: messages.as_array().unwrap().clone(),
            stream: false,
            temperature: Some(0.5),
            max_tokens: Some(2048),
            extra_params: json!({}),
        }
    }

    #[test]
    fn translate_request_includes_anthropic_version_and_omits_model() {
        let r = req(json!([{"role": "user", "content": "Hi"}]));
        let body = translate_request(&r);
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert!(body.get("model").is_none(), "model must live in URL, not body");
        assert_eq!(body["max_tokens"], 2048);
    }

    #[test]
    fn translate_request_extracts_system_text() {
        let r = req(json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "Hi"}
        ]));
        let body = translate_request(&r);
        assert_eq!(body["system"], "Be brief.");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn translate_request_defaults_max_tokens_when_missing() {
        let mut r = req(json!([{"role": "user", "content": "Hi"}]));
        r.max_tokens = None;
        let body = translate_request(&r);
        assert!(body["max_tokens"].as_u64().unwrap() > 0, "Anthropic requires max_tokens");
    }

    #[test]
    fn parse_response_extracts_text_and_usage() {
        let resp = json!({
            "content": [{"type": "text", "text": "Hello!"}],
            "usage": {"input_tokens": 9, "output_tokens": 2},
            "stop_reason": "end_turn"
        });
        let cr = parse_response(resp).unwrap();
        assert_eq!(cr.content, "Hello!");
        assert_eq!(cr.prompt_tokens, 9);
        assert_eq!(cr.completion_tokens, 2);
        assert_eq!(cr.finish_reason, "end_turn");
    }

    #[test]
    fn translate_sse_content_delta_becomes_openai_chunk() {
        let line = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        let out = translate_sse_line(line).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(r#""delta":{"content":"Hi"}"#));
    }

    #[test]
    fn translate_sse_message_stop_emits_done() {
        let line = r#"data: {"type":"message_stop"}"#;
        let out = translate_sse_line(line).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("[DONE]"));
    }
}
```

**Step 2:** Run — expect module-not-found

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -10
```

**Step 3:** Create `src/providers/vertex/claude.rs`:

```rust
use bytes::Bytes;
use crate::providers::adapter::{CompletionResult, NormalizedRequest};
use crate::providers::anthropic::translate_messages;

pub const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub fn translate_request(req: &NormalizedRequest) -> serde_json::Value {
    let (system_text, messages) = translate_messages(&req.messages);

    let mut body = serde_json::json!({
        "anthropic_version": VERTEX_ANTHROPIC_VERSION,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
    });
    if let Some(system) = system_text {
        body["system"] = serde_json::json!(system);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = serde_json::json!(t);
    }
    body
}

pub fn parse_response(v: serde_json::Value) -> anyhow::Result<CompletionResult> {
    let content: String = v["content"].as_array()
        .map(|arr| {
            arr.iter()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = &v["usage"];
    Ok(CompletionResult {
        content,
        prompt_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
        finish_reason: v["stop_reason"].as_str().unwrap_or("end_turn").to_string(),
    })
}

/// Translate a single Anthropic SSE line to an OpenAI chunk. Emits `[DONE]` at `message_stop`.
pub fn translate_sse_line(line: &str) -> Option<Bytes> {
    let payload = line.strip_prefix("data: ")?;
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match v["type"].as_str()? {
        "content_block_delta" if v["delta"]["type"] == "text_delta" => {
            let text = v["delta"]["text"].as_str()?;
            let chunk = serde_json::json!({
                "id": "chatcmpl-vertex-stream",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
            });
            Some(Bytes::from(format!("data: {}\n\n", chunk)))
        }
        "message_stop" => {
            let chunk = serde_json::json!({
                "id": "chatcmpl-vertex-stream",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            Some(Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", chunk)))
        }
        _ => None,
    }
}
```

**Step 4:** Add `pub mod claude;` to `src/providers/vertex/mod.rs`.

**Step 5:** Run tests — expect all pass

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -15
```

**Step 6:** Commit

```bash
git add src/providers/vertex/claude.rs src/providers/vertex/mod.rs tests/test_vertex.rs
git commit -m "feat(vertex): claude-on-vertex request/response/SSE translation"
```

---

## Task 6: Auth module with cached token provider

**Files:**
- Create: `src/providers/vertex/auth.rs`
- Modify: `src/providers/vertex/mod.rs`
- Test: `tests/test_vertex.rs`

**Design:** A `TokenProvider` trait (`async fn token() -> Result<String>`), plus two impls:
- `GoogleCloudAuthProvider` — real, uses `google-cloud-auth`.
- `StaticTokenProvider` (test-only, `#[cfg(test)]` not needed — keep in public auth module behind a `pub` export so integration tests can use it).

**Step 1:** Add a minimal trait test:

```rust
#[cfg(feature = "vertex")]
mod auth_tests {
    use modelrouter::providers::vertex::auth::{StaticTokenProvider, TokenProvider};

    #[tokio::test]
    async fn static_provider_returns_configured_token() {
        let p = StaticTokenProvider::new("ya29.abc".into());
        assert_eq!(p.token().await.unwrap(), "ya29.abc");
    }
}
```

**Step 2:** Run — expect failure

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -10
```

**Step 3:** Create `src/providers/vertex/auth.rs`:

```rust
use async_trait::async_trait;

#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn token(&self) -> anyhow::Result<String>;
}

/// Static token provider for tests and short-lived experiments.
pub struct StaticTokenProvider(String);

impl StaticTokenProvider {
    pub fn new(token: String) -> Self { Self(token) }
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

// Real google-cloud-auth provider. Constructed with an optional SA JSON path;
// when None, falls back to ADC (gcloud auth application-default login, metadata server, etc.).
pub struct GoogleCloudAuthProvider {
    credentials: google_cloud_auth::credentials::Credentials,
}

impl GoogleCloudAuthProvider {
    pub async fn new(credentials_path: Option<&str>) -> anyhow::Result<Self> {
        use google_cloud_auth::credentials::Builder;
        let mut builder = Builder::default();
        if let Some(path) = credentials_path {
            builder = builder.with_credentials_path(path);
        }
        let credentials = builder.build().await
            .map_err(|e| anyhow::anyhow!("failed to build GCP credentials: {e}"))?;
        Ok(Self { credentials })
    }
}

const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

#[async_trait]
impl TokenProvider for GoogleCloudAuthProvider {
    async fn token(&self) -> anyhow::Result<String> {
        let token = self.credentials
            .token(&[CLOUD_PLATFORM_SCOPE])
            .await
            .map_err(|e| anyhow::anyhow!("failed to fetch GCP access token: {e}"))?;
        Ok(token.value)
    }
}
```

**Note:** `google-cloud-auth`'s exact API may differ at the pinned version. If `cargo build --features vertex` fails in this file, run `cargo doc --features vertex --open` on the crate to find the correct builder/token API and adjust. The `TokenProvider` trait boundary insulates the rest of the code from churn.

**Step 4:** Add `pub mod auth;` to `src/providers/vertex/mod.rs`.

**Step 5:** Run

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -15
```

**Step 6:** Commit

```bash
git add src/providers/vertex/auth.rs src/providers/vertex/mod.rs tests/test_vertex.rs
git commit -m "feat(vertex): TokenProvider trait + google-cloud-auth + static (test) impls"
```

---

## Task 7: VertexAdapter — complete() + stream()

**Files:**
- Create: `src/providers/vertex/adapter.rs`
- Modify: `src/providers/vertex/mod.rs`
- Test: `tests/test_vertex.rs` (pure URL-building tests only; no real HTTP)

**Step 1:** Add URL-building unit tests:

```rust
#[cfg(feature = "vertex")]
mod adapter_tests {
    use modelrouter::providers::vertex::adapter::build_endpoint_url;
    use modelrouter::providers::vertex::dispatch::Publisher;

    #[test]
    fn gemini_non_streaming_url() {
        let url = build_endpoint_url("my-proj", "us-central1", Publisher::Google, "gemini-2.5-pro", false);
        assert_eq!(url, "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.5-pro:generateContent");
    }

    #[test]
    fn gemini_streaming_url_uses_sse_alt() {
        let url = build_endpoint_url("p", "us-central1", Publisher::Google, "gemini-2.5-flash", true);
        assert!(url.ends_with(":streamGenerateContent?alt=sse"));
    }

    #[test]
    fn anthropic_non_streaming_url_with_version_pin() {
        let url = build_endpoint_url("p", "us-east5", Publisher::Anthropic, "claude-sonnet-4-6@20250514", false);
        assert!(url.ends_with("/publishers/anthropic/models/claude-sonnet-4-6@20250514:rawPredict"));
    }

    #[test]
    fn anthropic_streaming_url() {
        let url = build_endpoint_url("p", "us-east5", Publisher::Anthropic, "claude-opus-4-5@20250101", true);
        assert!(url.ends_with(":streamRawPredict"));
    }
}
```

**Step 2:** Run — expect failure

```bash
cargo test --features vertex --test test_vertex 2>&1 | tail -10
```

**Step 3:** Create `src/providers/vertex/adapter.rs`:

```rust
use std::sync::Arc;
use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};
use crate::providers::vertex::auth::{GoogleCloudAuthProvider, TokenProvider};
use crate::providers::vertex::dispatch::{parse_model_id, Publisher};
use crate::providers::vertex::{claude, gemini};

/// Build the full Vertex REST URL for a given (project, region, publisher, model).
/// For Gemini streaming, appends `?alt=sse` so the server emits line-framed SSE.
pub fn build_endpoint_url(
    project: &str,
    region: &str,
    publisher: Publisher,
    model: &str,
    streaming: bool,
) -> String {
    let (pub_segment, method) = match (publisher, streaming) {
        (Publisher::Google, false) => ("google", "generateContent"),
        (Publisher::Google, true) => ("google", "streamGenerateContent"),
        (Publisher::Anthropic, false) => ("anthropic", "rawPredict"),
        (Publisher::Anthropic, true) => ("anthropic", "streamRawPredict"),
    };
    let mut url = format!(
        "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/{pub_segment}/models/{model}:{method}"
    );
    if matches!(publisher, Publisher::Google) && streaming {
        url.push_str("?alt=sse");
    }
    url
}

pub struct VertexAdapter {
    project: String,
    region: String,
    token_provider: Arc<dyn TokenProvider>,
    client: reqwest::Client,
}

impl VertexAdapter {
    pub async fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let project = config.project.clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex provider requires `project` in config"))?;
        let region = config.region.clone()
            .ok_or_else(|| anyhow::anyhow!("Vertex provider requires `region` in config"))?;
        let token_provider = Arc::new(
            GoogleCloudAuthProvider::new(config.credentials_path.as_deref()).await?
        ) as Arc<dyn TokenProvider>;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()?;
        Ok(Self { project, region, token_provider, client })
    }

    /// Test hook: build an adapter with an injected token provider (no Google OAuth).
    pub fn with_token_provider(
        project: String, region: String,
        token_provider: Arc<dyn TokenProvider>,
        timeout_secs: u64,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()?;
        Ok(Self { project, region, token_provider, client })
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for VertexAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let (publisher, model) = parse_model_id(&req.model)?;
        let url = build_endpoint_url(&self.project, &self.region, publisher, &model, false);
        let body = match publisher {
            Publisher::Google => gemini::translate_request(req),
            Publisher::Anthropic => claude::translate_request(req),
        };
        let token = self.token_provider.token().await?;
        let resp = self.client.post(&url)
            .bearer_auth(token)
            .json(&body).send().await
            .context("Failed to send request to Vertex AI")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Vertex AI returned {}: {}", status, text);
        }
        let v: serde_json::Value = resp.json().await
            .context("Failed to parse Vertex response")?;
        match publisher {
            Publisher::Google => gemini::parse_response(v),
            Publisher::Anthropic => claude::parse_response(v),
        }
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let (publisher, model) = parse_model_id(&req.model)?;
        let url = build_endpoint_url(&self.project, &self.region, publisher, &model, true);
        let body = match publisher {
            Publisher::Google => gemini::translate_request(req),
            Publisher::Anthropic => claude::translate_request(req),
        };
        let token = self.token_provider.token().await?;
        let resp = self.client.post(&url)
            .bearer_auth(token)
            .json(&body).send().await
            .context("Failed to send streaming request to Vertex AI")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Vertex AI returned {}: {}", status, text);
        }

        let stream = resp.bytes_stream()
            .map_err(|e| anyhow::anyhow!("Stream error: {}", e))
            .map_ok(move |chunk| {
                let text = String::from_utf8_lossy(&chunk);
                let mut out = String::new();
                for line in text.lines() {
                    let translated = match publisher {
                        Publisher::Google => gemini::translate_sse_line(line),
                        Publisher::Anthropic => claude::translate_sse_line(line),
                    };
                    if let Some(b) = translated {
                        out.push_str(&String::from_utf8_lossy(&b));
                    }
                }
                Bytes::from(out)
            });
        Ok(Box::pin(stream))
    }
}
```

**Step 4:** Update `src/providers/vertex/mod.rs`:

```rust
//! GCP Vertex AI provider (--features vertex).
pub mod adapter;
pub mod auth;
pub mod claude;
pub mod dispatch;
pub mod gemini;

pub use adapter::VertexAdapter;
```

**Step 5:** Run tests

```bash
cargo test --features vertex 2>&1 | tail -15
```

Expected: all vertex tests pass, full suite still green.

**Step 6:** Commit

```bash
git add src/providers/vertex/ tests/test_vertex.rs
git commit -m "feat(vertex): VertexAdapter with complete() + stream()"
```

---

## Task 8: Register provider in registry

**Files:**
- Modify: `src/providers/registry.rs:38-57`

**Step 1:** Add feature-gated branch in `ProviderRegistry::get` after the `azure` match (around line 40). The Vertex adapter's `new` is async, so mirror the bedrock pattern (lines 43-55):

```rust
        } else if provider_name == "azure" {
            Arc::new(crate::providers::azure_openai::AzureOpenAIAdapter::new(config))
        } else {
            #[cfg(feature = "vertex")]
            if provider_name == "vertex" {
                let vertex = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(crate::providers::vertex::VertexAdapter::new(config))
                })?;
                let entry = self
                    .adapters
                    .entry(provider_name.to_string())
                    .or_insert(Arc::new(vertex));
                return Ok(entry.clone());
            }
            #[cfg(feature = "bedrock")]
            if provider_name == "bedrock" {
                // ... existing bedrock block unchanged
```

**Step 2:** Sanity build

```bash
cargo build --features vertex 2>&1 | tail -5
cargo build 2>&1 | tail -5
cargo build --features bedrock 2>&1 | tail -5
cargo build --features "vertex,bedrock" 2>&1 | tail -5
```
All four should succeed.

**Step 3:** Commit

```bash
git add src/providers/registry.rs
git commit -m "feat(vertex): register VertexAdapter in provider registry"
```

---

## Task 9: Pricing for Gemini 2.5 + Claude-on-Vertex

**Files:**
- Modify: `src/router/cost.rs` (after the existing `gemini-1.5-*` rows at line 62-69)
- Test: `tests/test_cost.rs` if it exists (check first); otherwise add to `tests/test_vertex.rs`

**Prices (public list, dollars per 1M tokens, verify against the GCP Vertex pricing page at build time):**
- `gemini-2.5-pro` — input `1.25`, output `10.0` (prompts ≤ 200K; use the cheaper tier for MVP)
- `gemini-2.5-flash` — input `0.30`, output `2.50`
- `gemini-2.5-flash-lite` — input `0.10`, output `0.40`
- `claude-opus-4-5@20250101` — input `15.0`, output `75.0`
- `claude-sonnet-4-6@20250514` — input `3.0`, output `15.0`
- `claude-haiku-4-5@20251001` — input `0.80`, output `4.0`

> **Verify prices before committing.** Check https://cloud.google.com/vertex-ai/generative-ai/pricing and the Anthropic-on-Vertex page. If prices have changed, update this table and the code in lockstep.

**Step 1:** Write a quick test:

```rust
#[test]
fn gemini_25_pro_has_pricing() {
    use modelrouter::router::cost::CostCalculator;
    let calc = CostCalculator::new();
    let cost = calc.calc("gemini-2.5-pro", 1_000_000, 1_000_000);
    assert!(cost > 0.0, "gemini-2.5-pro must have non-zero pricing");
}
```

(Confirm the public method name is `calc` by reading `src/router/cost.rs`; adjust if different.)

**Step 2:** Add the rows to `CostCalculator::new` in `src/router/cost.rs`.

**Step 3:** Run

```bash
cargo test 2>&1 | tail -10
```

**Step 4:** Commit

```bash
git add src/router/cost.rs tests/
git commit -m "feat(vertex): pricing for Gemini 2.5 and Claude-on-Vertex"
```

---

## Task 10: Docs + example config

**Files:**
- Modify: `config.example.toml`
- Modify: `docs/local-setup.md`

**Step 1:** Append to `config.example.toml` after the `[providers.ollama]` block:

```toml
[providers.vertex]
project          = "my-gcp-project"
region           = "us-east5"               # must match a region where your models are available
credentials_path = "/secrets/sa.json"       # omit to use Application Default Credentials
timeout_secs     = 120

# Example aliases for Claude + Gemini on Vertex.
# Claude-on-Vertex models must be versioned — confirm current IDs in GCP Console.
# [routing.model_aliases]
# "gemini-2.5-pro"     = "vertex/google/gemini-2.5-pro"
# "gemini-2.5-flash"   = "vertex/google/gemini-2.5-flash"
# "claude-sonnet-4-6"  = "vertex/anthropic/claude-sonnet-4-6@20250514"
# "claude-opus-4-5"    = "vertex/anthropic/claude-opus-4-5@20250101"
```

**Step 2:** Add a Vertex section to `docs/local-setup.md` (before Step 6). Include:
- How to get a service-account JSON (IAM role `roles/aiplatform.user`)
- How to mount the JSON into the Docker container
- How to set `GOOGLE_APPLICATION_CREDENTIALS` env var as an alternative
- Note that Claude-on-Vertex requires region `us-east5` (or another supported region)
- Note that modelrouter must be **built with `--features vertex`** (not in the default image)

**Step 3:** Commit

```bash
git add config.example.toml docs/local-setup.md
git commit -m "docs(vertex): example config and local-setup guide"
```

---

## Task 11: Build verification

**Step 1:** Build all feature matrices

```bash
cargo build --release 2>&1 | tail -5                       # default
cargo build --release --features vertex 2>&1 | tail -5     # vertex alone
cargo build --release --features otel 2>&1 | tail -5       # otel alone
cargo build --release --features "vertex,otel" 2>&1 | tail -5  # combined (our target)
cargo build --release --features "vertex,otel,postgres,bedrock,prometheus" 2>&1 | tail -5
```

All five must succeed.

**Step 2:** Run full test suite in both default and vertex modes

```bash
cargo test 2>&1 | tail -5
cargo test --features vertex 2>&1 | tail -5
```

**Step 3:** (Optional but valuable) Build the Docker image with vertex + otel

```bash
docker build --build-arg FEATURES=vertex,otel -t modelrouter:vertex-otel .
```

If the Dockerfile bakes the zscaler CA (it does), the existing `certs/` works. No Dockerfile edits needed for this feature.

**Step 4:** Tag the PR-ready commit

```bash
git log --oneline main..HEAD
```

Confirm the sequence of commits matches the tasks above. Open a PR.

---

## Open items to revisit after MVP

- **Llama-on-Vertex** (publisher `meta`, OpenAI-compat body shape at publisher endpoint). Add a `llama.rs` translator symmetric to `gemini.rs`/`claude.rs` and extend `dispatch::Publisher`.
- **Progressive streaming token usage.** Gemini's `usageMetadata` arrives only on the final SSE event. Today we emit deltas as-is; consider accumulating and re-injecting a final `chunk` with `usage` when OpenAI clients pass `stream_options.include_usage: true`.
- **Retry + backoff on 429.** Current impl surfaces 429 straight out — this is fine because modelrouter has `fallback_chains`. If a Vertex-only deployment wants in-provider retries, add an exponential-backoff wrapper around the reqwest call.
- **Workload identity federation** (`google-cloud-auth` lists it as "coming soon"). Trivial to add once the crate supports it — construct a different credentials variant in `GoogleCloudAuthProvider::new`.
- **Pricing drift.** Gemini and Claude both have complex tiered pricing (≤200K tokens vs >200K, context caching discounts). Current pricing is flat — good enough for budget estimates, not for true cost attribution.
