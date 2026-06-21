# modelrouter — Hooks Reference

modelrouter has two hook systems: **lifecycle hooks** for fire-and-forget observability events, and **pipeline hooks** for synchronous request/response mutation. Both run external executables, which can be any language as long as the binary is in `$PATH` or specified by absolute path.

---

## Table of Contents

- [Hook Types](#hook-types)
  - [Lifecycle Hooks](#lifecycle-hooks)
  - [Pipeline Hooks](#pipeline-hooks)
- [Security Model](#security-model)
- [Configuration Reference](#configuration-reference)
- [Example: Headroom Context Compression](#example-headroom-context-compression)
  - [Why CCR is disabled — and what it would take to enable it](#why-ccr-is-disabled--and-what-it-would-take-to-enable-it)
- [Example: Not Diamond ML Router](#example-not-diamond-ml-router)
- [Example: Synthetic Session IDs for Opaque Clients](#example-synthetic-session-ids-for-opaque-clients)

---

## Hook Types

### Lifecycle Hooks

Lifecycle hooks fire **asynchronously** after key events. modelrouter does not wait for them to complete and ignores their output — they are purely observational.

| Event | When it fires | Payload fields |
|---|---|---|
| `on_request_received` | After auth + policy check, before routing | `event`, `user_name`, `model`, `message_count`, `timestamp` |
| `on_response_sent` | After the upstream response is returned to the caller | `event`, `user_name`, `model`, `routed_model`, `cost_usd`, `latency_ms` |
| `on_budget_exceeded` | When a budget limit blocks a request | `event`, `user_name`, `model`, `limit_usd`, `spent_usd`, `window` |

**Payload delivery:** The JSON payload is written to the hook's stdin. stdout is ignored. Non-zero exit and stderr are logged at WARN level.

**Config:**

```toml
[[hooks.lifecycle]]
name         = "my-observer"
event        = "on_response_sent"
exec         = "/usr/local/bin/my-observer"
timeout_secs = 10          # default 5; hook is killed after this many seconds
```

### Pipeline Hooks

Pipeline hooks run **synchronously** in the request path. The hook reads the full request (or response) body from stdin and writes a (possibly mutated) JSON body to stdout. modelrouter uses stdout as the new body for the rest of the pipeline.

| Event | When it fires | Can mutate |
|---|---|---|
| `request.pre` (config: `pre_request`) | After auth + policy, before provider routing | `messages`, `model`, `session_id`, and any other body fields |
| `response.post` (config: `post_response`) | After the upstream response, before returning to caller | Response body fields |

**Payload for `pre_request`:** The full OpenAI-compatible request body, plus modelrouter-injected fields:

| Field | Type | Description |
|---|---|---|
| `model` | string | Requested model (before alias resolution) |
| `messages` | array | Conversation messages |
| `session_id` | string \| null | Session stickiness ID, if provided by the caller |
| `_mr_session_window_secs` | integer | Per-key synthetic session window (default 28800) |
| `_mr_api_key_id` | integer \| null | ID of the authenticating API key |

**Payload delivery:** The JSON body is written to stdin. The hook must write valid JSON to stdout. If the hook exits non-zero or times out, behavior depends on `fail_open`.

**Config:**

```toml
[[hooks.pipeline]]
name         = "my-hook"
event        = "pre_request"            # or "post_response"
exec         = "/usr/local/bin/my-hook"
capabilities = ["mutate_request"]       # required to actually mutate; omit for read-only
timeout_secs = 5                        # hook killed after this; keep tight — adds latency
fail_open    = true                     # true = use original body on failure; false = block request
```

**Timeout budget:** Pipeline hooks add latency to every request. Keep `timeout_secs` as low as is safe — 3–5 seconds is typical. If the hook exceeds the budget, `fail_open = true` silently passes the original body through; `fail_open = false` returns an error to the caller.

---

## Security Model

Pipeline hooks that mutate requests or responses require an **operator grant** in the database. This prevents a misconfigured or malicious hook from mutating traffic without explicit authorization.

**Step 1 — Declare capabilities in config.toml:**

```toml
[[hooks.pipeline]]
name         = "headroom-compress"
capabilities = ["mutate_request"]
```

**Step 2 — Grant the capability as an operator:**

```sql
-- Run against your modelrouter database (SQLite or Postgres)
INSERT INTO hook_permissions (hook_name, capability, granted_at)
VALUES ('headroom-compress', 'mutate_request', datetime('now'));
```

Or via the modelrouter SQLite CLI:

```bash
sqlite3 ~/.modelrouter/router.db \
  "INSERT OR IGNORE INTO hook_permissions (hook_name, capability, granted_at) \
   VALUES ('headroom-compress', 'mutate_request', datetime('now'));"
```

At startup, modelrouter logs a warning for any hook that declares a capability without a matching grant. Hooks without grants are silently skipped — they do not block traffic and do not error.

Read-only hooks (no `capabilities` declared) require no grant and always run.

---

## Configuration Reference

Full `config.toml` example showing both hook types together:

```toml
[server]
port = 8080

[routing]
default_provider = "anthropic"
default_model    = "anthropic/claude-opus-4-5"

# ── Lifecycle hooks (fire-and-forget) ────────────────────────────────────────

[[hooks.lifecycle]]
name         = "cost-logger"
event        = "on_response_sent"
exec         = "/usr/local/bin/mr-cost-logger"
timeout_secs = 5

[[hooks.lifecycle]]
name         = "budget-alert"
event        = "on_budget_exceeded"
exec         = "/usr/local/bin/mr-budget-alert"
timeout_secs = 5

# ── Pipeline hooks (synchronous, in-request path) ────────────────────────────

[[hooks.pipeline]]
name         = "headroom-compress"
event        = "pre_request"
exec         = "/usr/local/bin/headroom-compress"
capabilities = ["mutate_request"]
timeout_secs = 5
fail_open    = true
```

---

## Example: Headroom Context Compression

[headroom](https://github.com/chopratejas/headroom) is a context compression library that reduces token counts by 60–95% before messages reach the upstream LLM. It removes redundant tool outputs, compresses prose and code, and deduplicates JSON arrays — without changing the message format that modelrouter and the upstream see.

The hook script at `docs/hooks/headroom-compress.py` implements a `request.pre` hook that calls a local headroom proxy and replaces `messages` in the request body with the compressed version.

### Architecture

```
Client
  │  POST /v1/chat/completions
  ▼
modelrouter
  │  auth + policy check
  │
  │  [request.pre hook fires]
  │    ├─ stdin:  full request body (with _mr_* fields)
  │    ├─ calls:  http://127.0.0.1:8787/v1/compress
  │    └─ stdout: body with compressed messages
  │
  │  route to provider
  ▼
Upstream LLM (Anthropic / OpenAI / etc.)
```

The headroom proxy runs as a separate process on the same host. It is not in the critical network path — if it is unreachable, `fail_open = true` causes modelrouter to pass the original (uncompressed) messages through without error.

### Install headroom

```bash
pip install "headroom-ai[proxy]"
```

### Start the headroom proxy with CCR disabled

```bash
HEADROOM_NO_CCR_INJECT_TOOL=1 headroom proxy --host 127.0.0.1 --port 8787
```

The `HEADROOM_NO_CCR_INJECT_TOOL=1` flag disables **CCR (Compress-Cache-Retrieve)** — see [Why CCR is disabled](#why-ccr-is-disabled-and-what-it-would-take-to-enable-it) below.

To start headroom at boot, use a systemd unit, launchd plist, or Docker:

```bash
# Simple background process (development)
HEADROOM_NO_CCR_INJECT_TOOL=1 headroom proxy --host 127.0.0.1 --port 8787 &

# systemd (Linux)
# See docs/hooks/headroom.service for a complete unit file example
```

Verify it is ready:

```bash
curl http://127.0.0.1:8787/health
# → {"status": "ok"}
```

### Install the hook script

```bash
cp docs/hooks/headroom-compress.py /usr/local/bin/headroom-compress
chmod +x /usr/local/bin/headroom-compress
```

The script uses only the Python standard library (`json`, `urllib`) — no additional dependencies.

### Configure modelrouter

Add to `~/.modelrouter/config.toml`:

```toml
[[hooks.pipeline]]
name         = "headroom-compress"
event        = "pre_request"
exec         = "/usr/local/bin/headroom-compress"
capabilities = ["mutate_request"]
timeout_secs = 5
fail_open    = true
```

### Grant the capability

```bash
sqlite3 ~/.modelrouter/router.db \
  "INSERT OR IGNORE INTO hook_permissions (hook_name, capability, granted_at) \
   VALUES ('headroom-compress', 'mutate_request', datetime('now'));"
```

### Optional environment variables

The hook script reads two environment variables. Set them in the shell that runs `modelrouter serve`, or in your systemd/launchd unit:

| Variable | Default | Description |
|---|---|---|
| `HEADROOM_URL` | `http://127.0.0.1:8787` | Base URL of the headroom proxy |
| `HEADROOM_TIMEOUT_SECS` | `4` | HTTP timeout for compress calls (should be < hook `timeout_secs`) |

### Verify compression is working

The hook writes a compression summary to stderr on every request, which modelrouter captures and logs at DEBUG level:

```
headroom-compress: 4821 → 1203 tokens (25% kept)
```

You can also query headroom's stats endpoint:

```bash
curl http://127.0.0.1:8787/stats
```

---

### Why CCR is disabled — and what it would take to enable it

**What CCR does:**

CCR (Compress-Cache-Retrieve) is headroom's reversible compression mode. Instead of lossy text compression, it stores the full original content locally and replaces it in the message with a short reference token (e.g., `<<ccr:a3f9b2c1d4e5>>`). If the LLM needs the original content during its response, it calls a `headroom_retrieve` tool that headroom injects into the tool list. The LLM gets everything it needs; the provider sees far fewer tokens per request.

CCR achieves higher savings than lossy compression (up to 95% vs. 60–80%) and is completely lossless from the model's perspective.

**Why it is off here:**

CCR stores originals in a **shared local cache** (SQLite by default). The cache key is `SHA-256(original_content)[:24]` — derived from content only, with no tenant namespace. In a multi-user modelrouter deployment, User A's cached content shares a namespace with User B's. This is a privacy concern for any deployment where users have sensitive or confidential data in their messages.

**What would be required to enable CCR safely in a shared deployment:**

Option 1 — **Per-project headroom instances** (recommended for O(10s) of projects):

Run one headroom proxy per project, each on its own port. The hook script reads `_mr_api_key_project` from the request body (injected by modelrouter) and routes the compress call to the correct instance. Each instance has an isolated cache.

```python
# In headroom-compress.py, replace the fixed URL with:
PROJECT_PORTS = {
    "codex-agent":    8787,
    "notebook-dev":   8788,
    "internal-tools": 8789,
}
project = body.get("_mr_api_key_project", "default")
port = PROJECT_PORTS.get(project, 8787)
url = f"http://127.0.0.1:{port}/v1/compress"
```

This is the cleanest solution. The only overhead is N headroom processes (each is lightweight — ~50 MB RSS), and each project's CCR cache is completely isolated. For O(10s) of projects, this is practical.

Option 2 — **Patch headroom to namespace cache keys** (required for O(100s+) of projects):

headroom's `CompressionStore.store()` in `headroom/cache/compression_store.py` accepts an `explicit_hash` parameter, but there is no extension point in the `PipelineExtensionManager` plugin system that fires between "request arrives" and "cache write" — the plugin events fire either too early (before compression) or too late (after the cache write has already occurred). Namespacing cache keys therefore requires a small patch to headroom itself:

```python
# compression_store.py (patch)
def store(self, original: str, ..., namespace: str | None = None) -> str:
    if explicit_hash is not None:
        hash_key = explicit_hash.lower()
    else:
        content = f"{namespace}:{original}" if namespace else original
        hash_key = hashlib.sha256(content.encode()).hexdigest()[:24]
```

The namespace value (e.g., the project name) would need to be threaded from the incoming HTTP request headers through the proxy handler into the compression pipeline — roughly 80 lines of Python across three files. This approach scales to any number of tenants on a single process, but requires maintaining a fork or upstreaming a PR to headroom.

**Summary:**

| Mode | Privacy | Token savings | Operational overhead |
|---|---|---|---|
| Single instance, CCR off (this example) | Full — stateless, no shared cache | Good (60–80%) | Minimal — one process |
| Per-project instances, CCR on | Full — isolated per project | Best (up to 95%) | Low — N lightweight processes |
| Single instance, CCR on + namespace patch | Full — namespaced cache | Best (up to 95%) | Low (ops) + Medium (one-time dev) |

For most modelrouter deployments, start with **CCR off** (this example). If the token savings from CCR justify the added setup, move to **per-project instances** — it is operationally simple and requires no headroom modifications.

---

## Example: Not Diamond ML Router

Not Diamond uses ML to route each request to the model most likely to give the best response for that query type. See `docs/hooks/not-diamond-router.md` for the full setup guide.

---

## Example: Synthetic Session IDs for Opaque Clients

Tools like Claude Code and Codex do not include `session_id` in their requests. A `request.pre` hook can inject a synthetic session ID derived from the API key and a rolling time window, enabling session stickiness for these clients.

The window size is configurable per key (see `modelrouter key create --session-window`) and is injected into the request body as `_mr_session_window_secs` before the hook runs.

```python
#!/usr/bin/env python3
import json, sys, time, hashlib

body = json.load(sys.stdin)
# _mr_session_window_secs comes from the key's configured value (default 28800 = 8 hours).
window = body.get("_mr_session_window_secs", 28800)
bucket = int(time.time()) // window
api_key_id = body.get("_mr_api_key_id", "default")
body.setdefault("session_id", hashlib.sha256(f"{api_key_id}:{bucket}".encode()).hexdigest()[:16])
json.dump(body, sys.stdout)
```

Configure per-project session windows when creating keys:

```bash
modelrouter key create --user alice --project codex-agent    --session-window 28800
modelrouter key create --user alice --project notebook-dev   --session-window 1800
modelrouter key create --user alice --project overnight-jobs --session-window 86400
```

See the [Session Stickiness](README.md#session-stickiness) section in the main README for full details.
