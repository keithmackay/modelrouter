# Phase 11d: AWS Bedrock Adapter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an AWS Bedrock provider adapter behind `--features bedrock` that translates OpenAI-format requests to the Bedrock Converse API and maps responses back.

**Architecture:** A feature-gated `BedrockAdapter` implementing `ProviderAdapter` using `aws-sdk-bedrockruntime`. Credentials come from the AWS standard chain (env vars / ~/.aws). Message translation uses pure functions that return `serde_json::Value` (unit-testable); SDK type construction uses builder APIs (AWS SDK types do not implement `serde::Deserialize`). The registry dispatches `"bedrock"` to `BedrockAdapter` when the feature is enabled. Streaming is implemented as collect-all-then-iterate (known limitation; real progressive streaming requires a follow-up task using `async-stream` or a tokio channel).

**Tech Stack:** `aws-sdk-bedrockruntime 1.x`, `aws-config 1.x`, Rust `#[cfg(feature = "bedrock")]`, existing `ProviderAdapter` trait, `async_trait`, `futures`, `bytes`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Add `bedrock` feature + optional AWS deps |
| `src/config/schema.rs` | Modify | Add `region: Option<String>` to `ProviderConfig` |
| `src/providers/bedrock.rs` | Create | `BedrockAdapter` implementing `ProviderAdapter` |
| `src/providers/mod.rs` | Modify | `#[cfg(feature = "bedrock")] pub mod bedrock;` |
| `src/providers/registry.rs` | Modify | Dispatch `"bedrock"` → `BedrockAdapter` (feature-gated) |
| `tests/test_bedrock.rs` | Create | Unit tests for translation functions (no AWS calls) |

---

### Task 1: Cargo.toml — bedrock feature flag + dependencies

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Write the failing build check**

Run: `cargo build --features bedrock 2>&1 | head -5`
Expected: error — unknown feature `bedrock`

- [ ] **Step 2: Add the feature and dependencies**

In `Cargo.toml`, add to `[features]`:
```toml
bedrock = ["dep:aws-sdk-bedrockruntime", "dep:aws-config"]
```

Add to `[dependencies]` (optional):
```toml
# ── AWS Bedrock (--features bedrock) ───────────────────────────────────
aws-sdk-bedrockruntime = { version = "1", optional = true }
aws-config             = { version = "1", optional = true }
```

- [ ] **Step 3: Verify the feature compiles (empty feature)**

Run: `cargo build --features bedrock 2>&1 | head -20`
Expected: compiles (no bedrock code exists yet, so no errors)

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat(bedrock): add bedrock feature flag and AWS SDK dependencies"
```

---

### Task 2: Config schema — add `region` to `ProviderConfig`

**Files:**
- Modify: `src/config/schema.rs`

- [ ] **Step 1: Write the failing test**

Create `tests/test_bedrock.rs`:
```rust
// tests/test_bedrock.rs

#[test]
fn provider_config_region_defaults_to_none() {
    let config = modelrouter::config::schema::ProviderConfig {
        api_key: String::new(),
        api_base: None,
        timeout_secs: 60,
        api_version: None,
        region: None,
    };
    assert!(config.region.is_none());
}
```

Run: `cargo test test_bedrock 2>&1 | head -20`
Expected: compile error — `ProviderConfig` has no field `region`

- [ ] **Step 2: Add `region` field to `ProviderConfig`**

In `src/config/schema.rs`, add the field to `ProviderConfig`:
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
    /// AWS region for Bedrock (e.g. "us-east-1"). Used only by the Bedrock adapter.
    /// Defaults to the AWS standard chain (AWS_REGION env var / ~/.aws/config).
    pub region: Option<String>,
}
```

- [ ] **Step 3: Fix struct literal compile errors in all test files**

Search for all `ProviderConfig {` struct literals:
```bash
grep -rn "ProviderConfig {" tests/ src/
```

For each literal that does NOT use `..Default::default()`, add `region: None`.

Known files to check (from prior phases):
- `tests/test_azure.rs`
- `tests/test_telemetry.rs` — gated behind `#![cfg(feature = "otel")]`; only surfaces when running `cargo test --features otel`

- [ ] **Step 4: Run tests with and without otel feature**

```bash
cargo test 2>&1 | tail -20
cargo test --features otel 2>&1 | tail -20
```
Expected: all pass both times

- [ ] **Step 5: Commit**

```bash
git add src/config/schema.rs tests/test_bedrock.rs tests/test_azure.rs tests/test_telemetry.rs
git commit -m "feat(bedrock): add region field to ProviderConfig"
```

---

### Task 3: BedrockAdapter — message translation (pure functions, unit tested)

**Files:**
- Create: `src/providers/bedrock.rs`
- Modify: `src/providers/mod.rs`
- Modify: `tests/test_bedrock.rs`

**Bedrock Converse API wire format:**
```json
{
  "modelId": "anthropic.claude-3-5-sonnet-20241022-v2:0",
  "messages": [{"role": "user", "content": [{"text": "Hello"}]}],
  "system": [{"text": "You are helpful."}],
  "inferenceConfig": {"maxTokens": 1024, "temperature": 0.7}
}
```

Rules for translation:
- Messages with `role == "system"` are extracted into the top-level `system` array; they must NOT appear in `messages`.
- Each non-system message's `content` is wrapped in `[{"text": "..."}]`.
- `temperature` and `max_tokens` go in `inferenceConfig`; omit key if `None`.
- The model ID is passed directly to the SDK `.model_id()` builder call.

**Important**: AWS SDK generated types do NOT implement `serde::Deserialize`. Do not use `serde_json::from_value::<SdkType>()`. Always use SDK builder APIs to construct SDK types.

- [ ] **Step 1: Write the failing tests for translation functions**

Add to `tests/test_bedrock.rs`:
```rust
#[cfg(feature = "bedrock")]
mod bedrock_translation {
    use modelrouter::providers::bedrock::{
        build_converse_messages, build_system_prompt, build_inference_config,
    };
    use serde_json::json;

    #[test]
    fn user_message_becomes_converse_format() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        let result = build_converse_messages(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn system_message_is_excluded_from_converse_messages() {
        let messages = vec![
            json!({"role": "system", "content": "Be helpful."}),
            json!({"role": "user", "content": "Hi"}),
        ];
        let converse = build_converse_messages(&messages);
        assert_eq!(converse.len(), 1);
        assert_eq!(converse[0]["role"], "user");
    }

    #[test]
    fn system_message_appears_in_system_prompt() {
        let messages = vec![
            json!({"role": "system", "content": "Be helpful."}),
            json!({"role": "user", "content": "Hi"}),
        ];
        let system = build_system_prompt(&messages);
        assert_eq!(system, vec![json!({"text": "Be helpful."})]);
    }

    #[test]
    fn no_system_message_returns_empty_system() {
        let messages = vec![json!({"role": "user", "content": "Hello"})];
        assert!(build_system_prompt(&messages).is_empty());
    }

    #[test]
    fn inference_config_with_both_fields() {
        let config = build_inference_config(Some(0.5), Some(100));
        assert_eq!(config["temperature"], 0.5);
        assert_eq!(config["maxTokens"], 100);
    }

    #[test]
    fn inference_config_with_no_fields_is_empty_object() {
        let config = build_inference_config(None, None);
        assert!(config.as_object().unwrap().is_empty());
    }
}
```

Run: `cargo test --features bedrock test_bedrock 2>&1 | head -30`
Expected: compile error — `providers::bedrock` module not found

- [ ] **Step 2: Add module declaration to `src/providers/mod.rs`**

```rust
#[cfg(feature = "bedrock")]
pub mod bedrock;
```

- [ ] **Step 3: Create `src/providers/bedrock.rs`**

```rust
// src/providers/bedrock.rs
//
// AWS Bedrock Converse API adapter.
//
// Credentials: standard AWS chain (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY /
// AWS_SESSION_TOKEN env vars, or ~/.aws/credentials + ~/.aws/config).
// Region: ProviderConfig.region, falling back to AWS_REGION env var / ~/.aws/config.
//
// Streaming limitation: the current `stream()` implementation collects all events
// before returning them. This means the HTTP response body does not begin sending
// until Bedrock finishes generating. Progressive streaming requires async-stream
// or a tokio channel and is left as a follow-up task.

use anyhow::Context;
use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::config::Region;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock,
};
use bytes::Bytes;
use futures::stream;

use crate::config::schema::ProviderConfig;
use crate::providers::adapter::{CompletionResult, NormalizedRequest, ProviderAdapter, SseStream};

// ── Translation helpers (pub so unit tests can import them) ─────────────────

/// Convert OpenAI-format messages to Bedrock Converse `messages` JSON array.
/// System messages are excluded — use `build_system_prompt` for those.
/// Returns `serde_json::Value` for easy unit testing; SDK types are built separately.
pub fn build_converse_messages(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| {
            let text = m["content"].as_str().unwrap_or("").to_string();
            serde_json::json!({
                "role": m["role"],
                "content": [{"text": text}],
            })
        })
        .collect()
}

/// Extract system messages into Bedrock `system` JSON array format.
pub fn build_system_prompt(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| {
            let text = m["content"].as_str().unwrap_or("").to_string();
            serde_json::json!({"text": text})
        })
        .collect()
}

/// Build a JSON `inferenceConfig` object. Used in unit tests and as reference.
/// Keys are omitted when None.
pub fn build_inference_config(
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(t) = temperature {
        map.insert("temperature".into(), serde_json::json!(t));
    }
    if let Some(m) = max_tokens {
        map.insert("maxTokens".into(), serde_json::json!(m));
    }
    serde_json::Value::Object(map)
}

// ── SDK type builders (not exported; used by complete/stream) ────────────────

/// Convert OpenAI-format messages into AWS SDK `Message` types using builder APIs.
/// AWS SDK types do NOT implement serde::Deserialize — JSON round-trip will not compile.
fn to_sdk_messages(messages: &[serde_json::Value]) -> anyhow::Result<Vec<Message>> {
    messages
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| {
            let role = match m["role"].as_str().unwrap_or("user") {
                "assistant" => ConversationRole::Assistant,
                _ => ConversationRole::User,
            };
            let text = m["content"].as_str().unwrap_or("").to_string();
            Message::builder()
                .role(role)
                .content(ContentBlock::Text(text))
                .build()
                .context("Failed to build Bedrock Message")
        })
        .collect()
}

/// Convert system messages into AWS SDK `SystemContentBlock` types.
fn to_sdk_system(messages: &[serde_json::Value]) -> Vec<SystemContentBlock> {
    messages
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| {
            let text = m["content"].as_str().unwrap_or("").to_string();
            SystemContentBlock::Text(text)
        })
        .collect()
}

// ── Adapter ──────────────────────────────────────────────────────────────────

pub struct BedrockAdapter {
    client: aws_sdk_bedrockruntime::Client,
}

impl BedrockAdapter {
    /// Construct a BedrockAdapter. This is async because aws_config::load is async.
    /// Called from ProviderRegistry::get via block_in_place + block_on.
    pub async fn new(config: &ProviderConfig) -> Self {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = &config.region {
            // Region lives in aws_sdk_bedrockruntime::config::Region (re-exported from aws-types)
            loader = loader.region(Region::new(region.clone()));
        }
        let aws_config = loader.load().await;
        let client = aws_sdk_bedrockruntime::Client::new(&aws_config);
        Self { client }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for BedrockAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        let sdk_messages = to_sdk_messages(&req.messages)?;
        let sdk_system = to_sdk_system(&req.messages);

        let mut builder = self
            .client
            .converse()
            .model_id(&req.model)
            .set_messages(Some(sdk_messages));

        if !sdk_system.is_empty() {
            builder = builder.set_system(Some(sdk_system));
        }

        // Build InferenceConfiguration with optional fields
        let mut inf_builder = InferenceConfiguration::builder();
        if let Some(t) = req.temperature {
            inf_builder = inf_builder.temperature(t as f32);
        }
        if let Some(m) = req.max_tokens {
            inf_builder = inf_builder.max_tokens(m as i32);
        }
        builder = builder.inference_config(inf_builder.build());

        let resp = builder
            .send()
            .await
            .context("Bedrock converse request failed")?;

        // Extract text from first content block of response message
        let content = resp
            .output()
            .and_then(|o| o.as_message().ok())
            .and_then(|m| m.content().first())
            .and_then(|b| b.as_text().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();

        // stop_reason() returns &StopReason (non-optional) — use as_str() directly
        let finish_reason = resp.stop_reason().as_str().to_string();

        // SDK returns i32 for token counts; clamp negatives to 0 defensively
        let (prompt_tokens, completion_tokens) = resp
            .usage()
            .map(|u| {
                (
                    u.input_tokens().max(0) as u32,
                    u.output_tokens().max(0) as u32,
                )
            })
            .unwrap_or((0, 0));

        Ok(CompletionResult {
            content,
            prompt_tokens,
            completion_tokens,
            finish_reason,
        })
    }

    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        // Known limitation: this implementation collects all Bedrock events before
        // returning the stream. The HTTP response will not start sending until
        // Bedrock finishes. Progressive streaming requires async-stream or a tokio
        // channel and is left as a follow-up task.
        let sdk_messages = to_sdk_messages(&req.messages)?;
        let sdk_system = to_sdk_system(&req.messages);

        let mut builder = self
            .client
            .converse_stream()
            .model_id(&req.model)
            .set_messages(Some(sdk_messages));

        if !sdk_system.is_empty() {
            builder = builder.set_system(Some(sdk_system));
        }

        let mut inf_builder = InferenceConfiguration::builder();
        if let Some(t) = req.temperature {
            inf_builder = inf_builder.temperature(t as f32);
        }
        if let Some(m) = req.max_tokens {
            inf_builder = inf_builder.max_tokens(m as i32);
        }
        builder = builder.inference_config(inf_builder.build());

        let mut event_stream = builder
            .send()
            .await
            .context("Bedrock converse_stream request failed")?
            .stream;

        let mut chunks: Vec<anyhow::Result<Bytes>> = Vec::new();

        loop {
            match event_stream.recv().await {
                Ok(Some(event)) => {
                    use aws_sdk_bedrockruntime::types::ConverseStreamOutput;
                    if let ConverseStreamOutput::ContentBlockDelta(delta_event) = event {
                        if let Some(delta) = delta_event.delta() {
                            if let Ok(text) = delta.as_text() {
                                let sse = format!(
                                    "data: {}\n\n",
                                    serde_json::json!({
                                        "choices": [{
                                            "delta": {"content": text},
                                            "finish_reason": null,
                                        }]
                                    })
                                );
                                chunks.push(Ok(Bytes::from(sse)));
                            }
                        }
                    }
                    // Non-text events (metadata, message start/stop) are silently skipped.
                }
                Ok(None) => break,
                Err(e) => {
                    chunks.push(Err(anyhow::anyhow!("Bedrock stream error: {}", e)));
                    break;
                }
            }
        }

        chunks.push(Ok(Bytes::from("data: [DONE]\n\n")));
        Ok(Box::pin(stream::iter(chunks)))
    }
}
```

- [ ] **Step 4: Run translation unit tests**

Run: `cargo test --features bedrock test_bedrock 2>&1 | tail -20`
Expected: all 6 translation tests pass

- [ ] **Step 5: Verify build without bedrock feature**

Run: `cargo test 2>&1 | tail -10`
Expected: all pass (bedrock code compiled out)

- [ ] **Step 6: Commit**

```bash
git add src/providers/bedrock.rs src/providers/mod.rs tests/test_bedrock.rs
git commit -m "feat(bedrock): add BedrockAdapter with Converse API and SDK builder types"
```

---

### Task 4: Registry dispatch — wire `"bedrock"` provider

**Files:**
- Modify: `src/providers/registry.rs`

Note: `ProviderRegistry::get()` is a sync method. `BedrockAdapter::new()` is async.
We use `tokio::task::block_in_place` + `block_on` to bridge this boundary.
This requires a multi-threaded Tokio runtime (axum uses `#[tokio::main]` with multi-thread by default).
Do NOT add `#[tokio::main(flavor = "current_thread")]` anywhere in the binary — that would panic.

Two concurrent requests for `"bedrock"` can both call `block_in_place` and construct a client before either inserts into `adapters`. The `or_insert` call ensures only the first constructed adapter is retained; the second is dropped. This is a wasted-work race (two AWS config loads), not a correctness bug.

- [ ] **Step 1: Add bedrock dispatch to `registry.rs`**

In `src/providers/registry.rs`, modify the adapter construction block to add a bedrock branch before the final `else`:

```rust
let adapter: Arc<dyn ProviderAdapter> = if provider_name == "anthropic" {
    Arc::new(crate::providers::anthropic::AnthropicAdapter::new(config))
} else if provider_name == "azure" {
    Arc::new(crate::providers::azure_openai::AzureOpenAIAdapter::new(config))
} else {
    #[cfg(feature = "bedrock")]
    if provider_name == "bedrock" {
        let bedrock = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(crate::providers::bedrock::BedrockAdapter::new(config))
        });
        // Use or_insert so concurrent callers don't create duplicate adapters
        let entry = self
            .adapters
            .entry(provider_name.to_string())
            .or_insert(Arc::new(bedrock));
        return Ok(entry.clone());
    }
    Arc::new(crate::providers::openai_compat::OpenAICompatAdapter::new(config))
};
```

- [ ] **Step 2: Build with bedrock feature to catch compile errors**

Run: `cargo build --features bedrock 2>&1 | tail -30`
Expected: compiles without errors

- [ ] **Step 3: Run all test suites**

```bash
cargo test 2>&1 | tail -10
cargo test --features bedrock 2>&1 | tail -10
cargo test --features otel 2>&1 | tail -10
```
Expected: all pass in all three configurations

- [ ] **Step 4: Commit**

```bash
git add src/providers/registry.rs
git commit -m "feat(bedrock): wire bedrock provider dispatch in registry"
```

---

### Task 5: Docs + final checks

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add bedrock build command to CLAUDE.md**

In `CLAUDE.md`, in the Testing section, add:
```markdown
cargo build --features bedrock  # Verify bedrock feature
```

- [ ] **Step 2: Full feature matrix test run**

```bash
cargo test 2>&1 | tail -10
cargo test --features otel 2>&1 | tail -10
cargo test --features bedrock 2>&1 | tail -10
cargo build --features postgres 2>&1 | tail -5
```
Expected: all pass / build succeeds

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add bedrock feature to CLAUDE.md build reference"
```

---

## Common Pitfalls

1. **`test_telemetry.rs` missing `region: None`** — this file is gated behind `#![cfg(feature = "otel")]` so `cargo test` passes but `cargo test --features otel` fails. Always run both after any `ProviderConfig` field change.

2. **AWS SDK types do NOT implement `serde::Deserialize`** — do not use `serde_json::from_value::<SdkType>()`. Always construct SDK types using their builder APIs (`.builder()...build()`).

3. **`Region` import path** — use `aws_sdk_bedrockruntime::config::Region`, not `aws_config::Region`. The latter does not exist in `aws-config 1.x`.

4. **`block_in_place` requires multi-thread runtime** — `ProviderRegistry::get()` is sync, but `BedrockAdapter::new()` is async. The `block_in_place` + `block_on` bridge works with axum's default multi-thread Tokio runtime. Never use `flavor = "current_thread"` on the binary entry point.

5. **Token counts are `i32` in the SDK** — `usage.input_tokens()` returns `i32`. Use `.max(0) as u32` when casting to avoid silent wrap on hypothetical negative values.

6. **Streaming is collect-all** — the `stream()` implementation collects all events before returning. This means the HTTP client receives no bytes until Bedrock finishes generating. This is a known limitation; progressive streaming requires a follow-up using `async-stream` or `tokio::sync::mpsc` + `ReceiverStream`.

7. **Non-exhaustive match on `ConverseStreamOutput`** — the enum is non-exhaustive. The `if let` pattern in the plan handles this; do not use `match` with explicit arms unless you include a `_ => {}` wildcard.
