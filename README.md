# modelrouter

![Release](https://img.shields.io/github/actions/workflow/status/keithmackay/modelrouter/release.yml?label=release)
![Version](https://img.shields.io/badge/version-0.1.1-blue)
![License](https://img.shields.io/badge/license-MIT-green)
![Rust](https://img.shields.io/badge/rust-2021-orange)

An OpenAI-compatible LLM proxy that routes requests across providers, enforces spend budgets at every scope — global, project, user, and group — and runs configurable hooks, all from a single self-hosted binary.

Point your existing OpenAI SDK at modelrouter instead of `api.openai.com`. It authenticates your users with API keys, resolves model aliases, selects the right upstream provider, tracks token spend, and stops requests that would blow a budget before they reach the provider.

---

## Table of Contents

- [Highlights](#highlights)
- [Getting Started](#getting-started)
- [Admin Setup Walkthrough](#admin-setup-walkthrough)
- [Developer Setup](#developer-setup)
- [Configuration](#configuration)
- [Usage](#usage)
- [Architecture](#architecture)
- [Development](#development)
- [Contributing](#contributing)
- [License](#license)

---

## Highlights

- **Drop-in OpenAI compatibility** — any SDK that speaks `POST /v1/chat/completions` works without modification
- **Multi-provider routing** — route to OpenAI, Anthropic, Google Gemini, or Ollama; switch providers by changing one config line
- **Streaming from every provider, with router-owned usage capture** — `"stream": true` yields OpenAI-shaped SSE from all upstreams (native Vertex Claude/Gemini and Bedrock streams are translated in flight); the router always obtains real token counts from the provider — independent of the client's request shape — meters streamed traffic exactly (budgets, attribution, experiment variants), and hands every caller a normalized final `usage` chunk
- **Routing shortcuts** — use `:fastest` or `:cheapest` as the model name to route to your configured fastest or cheapest model without changing client code
- **Runtime model aliases** — point an alias like `deep` at any `provider/model` from the admin UI or CLI, with no restart; DB aliases override config, and resolution is depth-capped so a cycle cannot hang a request
- **Operator disable** — take a model or a whole provider out of rotation with a recorded reason; disabled targets return **403** naming the reason instead of reaching the provider, and stay disabled until explicitly re-enabled (unlike a circuit-breaker trip)
- **Failure capture** — every request that does NOT return a result is persisted with the stage it died at (`resolve` / `policy` / `provider` / `request` / `internal`) and surfaced at `/admin/failures`; prompt rows only ever record successes, so without this the router had no answer for "what failed"
- **Strict model resolution** — refuse to silently substitute `default_model` for a model nobody configured, instead of answering with a different model than the caller asked for
- **Embedding failover with width verification** — `/v1/embeddings` walks the same fallback chains as completions, and a `dimensions` request is verified against what the provider returned, so failing over between models of different widths can never corrupt a vector store
- **Multi-scope budget enforcement** — set monthly or fixed date-range limits at the global (org-wide), project, user, or group level; any limit hit blocks the request before it reaches the upstream
- **Admin dashboard** — web UI at `/admin` with usage stats, failures, audit log, and full management pages for users, API keys, groups, budgets, and webhooks
- **Webhook callbacks** — register outbound webhooks via admin UI or CLI (`modelrouter webhook add`) that fire JSON POSTs after each completion; wire Datadog, Slack, or any HTTP endpoint; takes effect on next restart
- **Response cache** — identical eligible requests (deterministic completions and search queries) are served from an in-memory or Redis store at zero provider cost; hits are metered with `cache_hit` and zero spend, and cache-hit % is a first-class metric on `/admin/cache` and the cost page
- **Controlled experiments** — create an experiment with named variants, put `x-modelrouter-experiment: <id>[:<label>]` on each chat completion, report outcomes with `POST /v1/feedback`, and read per-variant and per-run cost, tokens, turns, latency, failures and outcomes back as JSON, from the CLI, or on `/admin/experiments`; bound requests are pinned to their variant's priced model with no downgrade, pool, cache or fallback
- **Request cost attribution** — tag any metered call with your own correlation id and free-form tags (`attribution` in the request body, or `X-Attribution-*` headers); the router persists them on the prompt and cost-ledger rows and reports spend *and* cache savings per tag, so a consuming app can drop its own cost accounting. Attribution never affects routing or the cache key
- **Subscription mode** — send Anthropic, OpenAI and Gemini chat traffic through `claude -p`, `codex exec` and `gemini` on their subscription logins, resuming one CLI session per conversation, and meter it like API traffic ([details](#subscription-mode-chat-through-the-vendor-clis))
- **Session stickiness** — include `session_id` in any request to pin the session to the winning provider; automatically re-pins on model change; opt out per-request with `X-Session-Lb: true`
- **Prompt logging control** — set `X-No-Log: true` on any request to skip prompt history and callback dispatch while preserving cost tracking for budget enforcement
- **Chinese model providers** — built-in pricing for DeepSeek, Qwen (Tongyi), and Doubao; all are OpenAI-compatible and configured as standard providers
- **Declarative policy engine** — TOML-configured rules that match users by project, group, or ID and enforce model allow-lists and budgets without touching the database
- **Content guardrails** — pluggable safety layer runs OpenAI moderation (or a custom HTTP endpoint) on requests and responses; configurable fail-open/fail-closed
- **MCP server registry** — register and discover Model Context Protocol servers via REST; semantic search ranks results by relevance to a query
- **SSO / OIDC** — admin users can authenticate via Google, Okta, Auth0, or any OIDC provider using authorization code flow with PKCE; new admins are auto-provisioned from email allow-lists
- **Hook system** — run shell scripts or HTTP webhooks at lifecycle events and in the request pipeline; grant capabilities per-user via `hook_permissions`
- **Feature-flagged optional components** — `--features postgres` for Postgres backend, `--features otel` for full OpenTelemetry observability (traces, metrics, logs via OTLP)
- **Single static binary** — SQLite bundled, no runtime dependencies; ships as a distroless Docker image

---

## Session Stickiness

When a request includes a `session_id` field, modelrouter pins that session to the upstream provider selected on the first request. Every subsequent request with the same `session_id` goes to the same provider — even if a load balancer pool is configured that would otherwise distribute traffic.

```json
{
  "model": "claude-opus-4-5",
  "session_id": "user-42-conv-891",
  "messages": [{"role": "user", "content": "Hello"}]
}
```

Pins expire after 30 minutes of inactivity and are stored in memory (not persisted across restarts).

### Why stickiness matters

Many providers offer prompt caching: if the same long prefix (system prompt, document, conversation history) appears in consecutive requests, the provider reuses its cached computation and charges a fraction of the normal input rate. Caches are local to a specific provider endpoint. Routing turn 3 of a conversation to a different provider than turns 1 and 2 produces a cache miss and charges full price.

Stickiness ensures that a session's accumulated context always lands on the same provider, keeping the cache warm.

### Model changes mid-session

If a request in a pinned session specifies a different model, modelrouter updates the pin rather than ignoring the change:

| Change | Behaviour |
|---|---|
| Same model, same provider | Use pin, refresh TTL |
| Different model, **same provider** | Keep provider pin, use new model, update pin |
| Different model, **different provider** | Clear old pin, route normally, store new pin |

Switching between two Anthropic models mid-session keeps traffic on Anthropic (preserving the provider relationship and any cached prefix), while switching from Claude to GPT-4o routes to OpenAI and starts a fresh pin.

Model comparisons use the **resolved** canonical model after alias and shortcut expansion — switching from `"opus"` to `"anthropic/claude-opus-4-5"` where `opus` is an alias is recognised as the same model and does not update the pin.

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

If you have a session open but a specific request is stateless and you want the load balancer to choose freely for that request, set `X-Session-Lb: true`:

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

The load balancer picks freely for that request, and the result becomes the new pin for the session going forward.

**Synthetic session IDs for opaque clients:**

Tools like Claude Code or Codex do not include `session_id` in their requests. To enable stickiness for these clients, use a `request.pre` pipeline hook to inject a synthetic `session_id` derived from the API key and a rolling time window:

```python
#!/usr/bin/env python3
import json, sys, time, hashlib

body = json.load(sys.stdin)
# _mr_session_window_secs is injected by modelrouter from the key's configured value.
# Default is 28800 (8 hours). Override per key via CLI or admin UI.
window = body.get("_mr_session_window_secs", 28800)
bucket = int(time.time()) // window
api_key_id = body.get("_mr_api_key_id", "default")
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

The window controls the maximum age of a synthetic session ID — not the affinity TTL, which refreshes on every request. Choose a window that matches the typical session duration for each project:

| Use case | Recommended window |
|---|---|
| Raw Python / notebook sessions | `1800` (30 min) |
| Claude Code / Codex agentic loops | `28800` (8 hours, default) |
| Long-running overnight jobs | `86400` (24 hours) |

Set the window when creating a key:

```bash
modelrouter key create --user alice --project codex-agent --session-window 28800
modelrouter key create --user alice --project notebook-dev --session-window 1800
```

Or in the admin dashboard under **Keys → Session Window (secs)**. The value is preserved when a key is rotated.

---

## Getting Started

### Prerequisites

- At least one upstream provider API key (OpenAI, Anthropic, Gemini, or a local Ollama instance)
- Rust 1.94.1+ if building from source (the AWS SDK crates set that floor)
- Optional: PostgreSQL 14+ if using `--features postgres`

### Installation

**Docker (from GHCR):**

Pick the image that matches the features you need:

| Image | Features |
|---|---|
| `ghcr.io/keithmackay/modelrouter:latest` | SQLite only |
| `ghcr.io/keithmackay/modelrouter:latest-otel` | + OpenTelemetry |
| `ghcr.io/keithmackay/modelrouter:latest-postgres` | + PostgreSQL |
| `ghcr.io/keithmackay/modelrouter:latest-full` | All features (OTel + Postgres + Bedrock + Prometheus) |

> **Note:** These images are hosted on a private GHCR package. Authenticate first:
> ```bash
> docker login ghcr.io -u <your-github-username> --password-stdin <<< <your-github-pat>
> ```
> A GitHub Personal Access Token with `read:packages` scope is required.

```bash
docker pull ghcr.io/keithmackay/modelrouter:latest
docker run -d --name modelrouter --restart unless-stopped \
  -v /host/config:/config:ro \
  -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -e MODELROUTER_DATABASE__PATH=/data/router.db \
  -p 127.0.0.1:8080:8080 \
  ghcr.io/keithmackay/modelrouter:latest serve --host 0.0.0.0
```

- `--host 0.0.0.0` is required: `[server] host` defaults to `127.0.0.1`, which inside a
  container is unreachable through the published port.
- `MODELROUTER_DATABASE__PATH` puts the SQLite file on the `/data` volume; the default
  (`~/.modelrouter/router.db`) lives in the container's filesystem and is lost with it.
- `/host/config/config.toml` must exist first — generate it with
  `modelrouter --config /host/config/config.toml init`.
- `-p 127.0.0.1:8080:8080` keeps the proxy and admin dashboard off the network; drop the
  `127.0.0.1:` to expose them.
- The release images are distroless (no shell), so run other commands as arguments:
  `docker exec -it modelrouter /modelrouter admin create --name ops --role superadmin`.

For Postgres and the OpenTelemetry stack under Docker Compose, see
[Running on PostgreSQL](#running-on-postgresql).

**Build from source:**

```bash
git clone https://github.com/keithmackay/modelrouter.git
cd modelrouter
cargo build --release
# Binary is at target/release/modelrouter
```

```bash
# With OTel support
cargo build --release --features otel

# With Postgres support
cargo build --release --features postgres
```

### If required: adding a corporate CA certificate (Zscaler, Netskope, etc.)

If your network uses SSL/TLS inspection (common with Zscaler, Netskope, Palo Alto GlobalProtect, or other corporate security proxies), outbound HTTPS from the modelrouter container will be intercepted and re-signed with your organisation's private CA certificate. The container's default trust store does not include that CA, so TLS verification fails and every provider call returns a 502.

**Symptoms:**
- `502 Bad Gateway` on every request
- Container logs show `Failed to send request to Anthropic` (or similar) with ~150–300ms latency
- `curl -sk https://api.anthropic.com/` works from inside the container (bypassing cert check), but `curl -s` fails with exit code 60

**Fix: extract the CA cert and inject it into the image at build time.**

1. **Export the CA certificate from your machine's trust store.**

   *macOS* — find and export the cert:
   ```bash
   # List candidates (look for your proxy vendor name)
   security find-certificate -a /Library/Keychains/System.keychain | grep "labl\|alis" | grep -i "zscaler\|netskope\|palo"

   # Export the one you find (adjust the name)
   security find-certificate -c "Zscaler Root CA" -p /Library/Keychains/System.keychain \
     > certs/zscaler-root-ca.pem
   ```

   *Linux* — typically found in `/etc/ssl/certs/` or exported via your MDM. Copy the `.pem` file to `certs/`.

   *Windows* — export from `certmgr.msc` (Trusted Root Certification Authorities → your proxy cert → Export → Base-64 encoded X.509).

2. **Place the PEM file at `certs/<name>.pem`** in the repository root. The `Dockerfile` copies everything from `certs/` into the image's CA bundle:

   ```dockerfile
   COPY --chown=root:root certs/zscaler-root-ca.pem /usr/local/share/ca-certificates/zscaler-root-ca.crt
   RUN update-ca-certificates
   ```

   Add additional files to `certs/` if your network has more than one inspection CA. Each file must have a `.pem` extension and contain a valid PEM-encoded certificate.

3. **Rebuild the image:**

   ```bash
   # Standard build
   docker build -t modelrouter:latest .

   # With OTel support (recommended)
   docker build --build-arg FEATURES="otel" -t modelrouter:otel -t modelrouter:latest .
   ```

4. **Restart the stack:**

   ```bash
   docker-compose -f docker-compose.otel.yml up -d --no-build
   ```

If TLS is still failing after adding the cert, run the built-in connectivity check:

```bash
docker-compose exec modelrouter ./modelrouter check-tls
```

This tests TLS connectivity to each configured provider and prints exactly which certificate in the chain is not trusted, so you know which CA to add.

---

## Admin Setup Walkthrough

This walkthrough takes a fresh modelrouter install to a fully configured team deployment: provider keys, superadmin, users, project keys, groups, and budgets.

### 1. Configure upstream provider keys

Edit `~/.modelrouter/config.toml`. Add your provider API keys and configure routing:

```toml
[providers.anthropic]
api_key = "sk-ant-..."
timeout_secs = 120

[providers.openai]
api_key = "sk-..."

[routing]
default_provider = "anthropic"
default_model = "claude-opus-4-6"

[routing.model_aliases]
"claude-code" = "anthropic/claude-opus-4-6"
```

See [`config.example.toml`](config.example.toml) for all provider options, guardrail definitions, policy rules, OIDC, and telemetry settings.

### Choosing the store: SQLite or PostgreSQL

SQLite is the default and right for one replica. Set `database.postgres_url` to run on PostgreSQL instead; `serve` and every CLI command then open that database and bring its schema to head on start (sqlx takes an advisory lock, so replicas starting together migrate once). The binary must be built with `--features postgres`; one built without it refuses a config that sets the URL rather than silently opening SQLite beside it.

```toml
[database]
postgres_url = "postgres://modelrouter:secret@db.internal:5432/modelrouter"
```

Still SQLite-only in this release: the `report` and `audit` CLI subcommands and the dashboard's hook statistics. On PostgreSQL they print `not available on the postgres backend yet` instead of a database error. To prove the backend against a real server locally:

```bash
MODELROUTER_TEST_POSTGRES_URL=postgres://user:pass@localhost/modelrouter_test \
  cargo test --features postgres --test test_postgres_live -- --ignored
```

### 2. Run migrations and start the server

```bash
modelrouter migrate
modelrouter serve
curl http://localhost:8080/health   # → {"status":"ok"}
```

`serve` binds the `host`/`port` from `[server]` in `config.toml`; `--host` and
`--port` override them for a single run.

### 3. Create the first superadmin

The first admin must be created via the CLI. All subsequent admin management is available in the web UI.

```bash
modelrouter admin create --name ops --role superadmin
# Password: (prompted on the terminal — there is no --password flag, so the
#            secret never lands in shell history or the process list)
# Admin 'ops' created (id=1, role=superadmin)
```

Log in at `http://localhost:8080/admin` with these credentials. Superadmin accounts can create additional admins, manage users, and configure budgets. Viewer accounts can read the dashboard but cannot mutate anything.

> **Optional OIDC SSO:** If your team uses Google, Okta, Auth0, or another OIDC provider, see [OIDC Configuration](#oidc-sso-for-admin-login) to let admins log in with their corporate credentials.

### 4. Create users

```bash
modelrouter user create --name abdoul
# Created user 'abdoul' (id=1)
# API key: mr-a1b2c3d4e5f6...
# Store this key securely — it cannot be retrieved later.

modelrouter user create --name becky
# Created user 'becky' (id=2)
# API key: mr-9z8y7x6w5v4u...

modelrouter user list
#    1  abdoul  enabled
#    2  becky   enabled
```

Each user gets a default API key at creation. Keys are shown exactly once — save them before closing the terminal. Users can also be managed in the **Admin → Users** dashboard page.

### 5. Create projects and issue per-project keys

A **project** is a label on an API key. Every request made with a project key is attributed to that project in the cost ledger, enabling per-project spend reports and budget enforcement.

```bash
# Issue Abdoul a key for the "modelrouter" project
modelrouter key create --user abdoul --project modelrouter --label "modelrouter dev — abdoul"
# → key: mr-xxxx...  project: modelrouter
# Save the key — it cannot be retrieved later.

modelrouter key create --user becky --project modelrouter --label "modelrouter dev — becky"
```

Share each key with the corresponding developer. See [Developer Setup](#developer-setup) for how they add it to their environment.

### 6. (Optional) Create groups

Groups collect users for spend tracking and reporting. A user can belong to multiple groups; spend is attributed to their highest-priority group.

```bash
# Create a group (priority 0 is default; higher number = higher priority)
modelrouter group create --name team-alpha --priority 0

# Add members
modelrouter group add-member --group team-alpha --user abdoul
modelrouter group add-member --group team-alpha --user becky

# Verify
modelrouter group members --group team-alpha
#  abdoul  joined 2026-04-10  Active
#  becky   joined 2026-04-10  Active
```

Groups can also be managed in the **Admin → Groups** dashboard page.

### 7. Configure budgets

Budgets are enforced independently — a request is blocked when *any* applicable rule is exceeded.

**Global limit** — hard ceiling on all org traffic:

```bash
# Monthly global cap
modelrouter budget set --global --window monthly --limit-usd 500

# Fiscal-year total cap
modelrouter budget set --global --window total --window-start 2026-04-01 --window-end 2026-06-30 --limit-usd 5000
```

**Project limits** — block all traffic on a project once its budget is hit:

```bash
modelrouter budget set --project modelrouter --window monthly --limit-usd 200
```

**User limits** — per-developer monthly or total spend caps:

```bash
modelrouter budget set --user abdoul --window monthly --limit-usd 50
modelrouter budget set --user becky  --window monthly --limit-usd 100
```

**Group targets** — informational only, never block requests:

```bash
modelrouter budget set --group team-alpha --limit-usd 300
```

Additional limit types compose freely:

```bash
# Rate limit + spend cap together
modelrouter budget set --user abdoul --window monthly --limit-usd 50 --rate-rpm 10

# Model allow-list (only these models accepted for this user)
modelrouter budget set --user abdoul --window monthly --limit-usd 50 \
  --model-allow claude-haiku-4-5,claude-sonnet-4-6
```

Review and manage rules:

```bash
modelrouter budget list
#   1  global         monthly       limit=$500.00
#   2  global         total         2026-04-01→2026-06-30  limit=$5000.00
#   3  project=modelrouter  monthly  limit=$200.00
#   4  user=abdoul    monthly       limit=$50.00  rpm=10
#   5  user=becky     monthly       limit=$100.00
#   6  group=team-alpha  target     limit=$300.00

modelrouter budget edit --id 4 --limit-usd 75
modelrouter budget delete --id 6
```

When a user hits their user limit, all their keys return `429 Budget exceeded` until the next monthly period. When a project or global limit is hit, all keys associated with that project (or all keys, for global) are blocked until the limit resets or is raised. Budgets can also be managed in the **Admin → Budgets** dashboard page.

---

## Developer Setup

Once an admin has created your account and issued you a project key, connecting your AI tools to modelrouter takes two steps.

### 1. Receive your key

The admin will give you:
- **A modelrouter URL** — e.g. `http://modelrouter.internal:8080`
- **A project key** — a `mr-...` token specific to you and your project

You may receive one key per project if your team tracks spend by project.

### 2. Set the key in your project's `.envrc`

[direnv](https://direnv.net/) automatically loads environment variables when you enter a directory, making it easy to use the right key for each project without manual switching.

**Install direnv (once):**

```bash
# macOS
brew install direnv

# Add to ~/.zshrc or ~/.bashrc
eval "$(direnv hook zsh)"   # or bash
```

**Create a `.envrc` in each project root:**

`~/Projects/my-project/.envrc`:
```bash
# Route AI tools through modelrouter for this project
export ANTHROPIC_BASE_URL="http://modelrouter.internal:8080"
export ANTHROPIC_API_KEY="mr-xxxx..."   # your key for this project

# Also set the OpenAI vars for tools that use the OpenAI SDK
export OPENAI_BASE_URL="http://modelrouter.internal:8080"
export OPENAI_API_KEY="mr-xxxx..."      # same key — modelrouter accepts both
```

```bash
# Allow the .envrc (once per directory)
cd ~/Projects/my-project && direnv allow
```

From this point, every AI tool in that shell session — Claude Code, Codex, Cursor, Continue, or any OpenAI-compatible SDK — automatically routes through modelrouter when you are working in that directory. When you leave the directory, the variables are unset.

> **Add `.envrc` to `.gitignore`** — it contains credentials and should never be committed.

### Multiple projects

If you work on multiple tracked projects, create a separate `.envrc` per project with the key for that project:

`~/Projects/other-project/.envrc`:
```bash
export ANTHROPIC_BASE_URL="http://modelrouter.internal:8080"
export ANTHROPIC_API_KEY="mr-yyyy..."   # your key for other-project
export OPENAI_BASE_URL="http://modelrouter.internal:8080"
export OPENAI_API_KEY="mr-yyyy..."
```

Changing directories automatically switches keys. No manual work needed.

### Opting specific projects in or out

**Default to direct Anthropic, opt specific projects into modelrouter:**

```bash
# ~/.zshrc — direct Anthropic everywhere by default
export ANTHROPIC_API_KEY="sk-ant-..."
```

```bash
# ~/Projects/work-project/.envrc — switch to modelrouter for this project
export ANTHROPIC_BASE_URL="http://modelrouter.internal:8080"
export ANTHROPIC_API_KEY="mr-xxxx..."
```

**Default to modelrouter, opt specific projects out:**

```bash
# ~/.zshrc — route everything through modelrouter by default
export ANTHROPIC_BASE_URL="http://modelrouter.internal:8080"
export ANTHROPIC_API_KEY="mr-default..."
```

```bash
# ~/Projects/personal-project/.envrc — revert to direct Anthropic
unset ANTHROPIC_BASE_URL
export ANTHROPIC_API_KEY="sk-ant-..."   # personal Anthropic key
```

### Tool-specific notes

**Claude Code** — reads `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` from the environment. No config file changes needed.

**OpenAI Codex CLI** — reads `OPENAI_BASE_URL` and `OPENAI_API_KEY`. modelrouter's `/v1` endpoint is fully OpenAI-compatible.

**OpenAI Python or Node SDK:**

```python
from openai import OpenAI
client = OpenAI(base_url="http://modelrouter.internal:8080/v1", api_key="mr-xxxx...")
```

```typescript
import OpenAI from "openai";
const client = new OpenAI({ baseURL: "http://modelrouter.internal:8080/v1", apiKey: "mr-xxxx..." });
```

**Cursor, Continue, and other tools** — use the "custom OpenAI base URL" or equivalent setting. Point it at `http://modelrouter.internal:8080` and use your modelrouter key as the API key.

---

## Configuration

Configuration lives at `~/.modelrouter/config.toml` by default, or at the path in `MODELROUTER_CONFIG`.

| Key | Description | Default |
|-----|-------------|---------|
| `server.host` | Bind address (`--host` overrides for one run) | `127.0.0.1` |
| `server.port` | Listen port (`--port` overrides for one run) | `8080` |
| `server.request_body_limit_mb` | Largest request body accepted; larger returns 413 | `10` |
| `database.path` | SQLite file path | `~/.modelrouter/router.db` |
| `database.postgres_url` | Use PostgreSQL instead of SQLite (requires `--features postgres`; see [Running on PostgreSQL](#running-on-postgresql)) | unset |
| `routing.default_provider` | Fallback provider when model prefix is absent | `openai` |
| `routing.model_aliases` | Map short names to canonical model IDs | — |
| `providers.<name>.api_key` | Upstream provider API key | required |
| `providers.<name>.base_url` | Override provider endpoint | provider default |
| `auth.jwt_secret` | Secret for admin JWT signing. `init` generates one; `serve` refuses to start if it is empty or the shipped placeholder | required |
| `[[guardrails]]` | Content safety rules (type, fail_open, api_key, endpoint) | — |
| `[[policy_rules]]` | Declarative access rules matched by project/group/user/model | — |
| `[oidc]` | OIDC SSO for admin login (issuer_url, client_id, client_secret, …) | disabled |
| `cache.enabled` | Turn the response cache on | `false` |
| `cache.backend` | `memory` (per-process) or `redis` (shared) | `memory` |
| `cache.completions.max_temperature` | Cache completions only at or below this temperature | `0.0` |
| `cache.search.ttl_seconds` | TTL for cached search results | `900` |
| `telemetry.endpoint` | OTLP gRPC endpoint (`--features otel`) | disabled |
| `telemetry.sample_ratio` | Fraction of normal requests to trace | `0.1` |

See [`config.example.toml`](config.example.toml) for a fully annotated reference configuration.

### Vertex AI: publishers, Model-as-a-Service, and the model catalog

The Vertex provider serves three distinct endpoint styles behind one `vertex/` prefix:

| Publisher | Endpoint style | Model id form |
|-----------|---------------|---------------|
| `google` (Gemini) | `models/{m}:generateContent` / `:streamGenerateContent` | `vertex/google/gemini-2.5-pro` |
| `anthropic` (Claude) | `models/{m}:rawPredict` / `:streamRawPredict` | `vertex/anthropic/claude-sonnet-4-5@…` |
| everything else (Mistral, Llama, DeepSeek, Qwen, …) | the shared OpenAI-compatible MaaS endpoint `endpoints/openapi/chat/completions` | `vertex/mistralai/mistral-medium-3` (full `publisher/model` id travels in the request body) |

Any publisher other than `google`/`anthropic` automatically routes through the MaaS arm —
no code change is needed to call a newly enabled Model Garden partner.

Vertex-specific config keys (all under `[providers.vertex]`; **publisher and region names
are operations data and live here, never in code**):

| Key | Description | Default |
|-----|-------------|---------|
| `project` | GCP project id | required |
| `region` | Chat region for google/anthropic (`global` is valid) | required |
| `embedding_region` | Embeddings are regional-only; `global` has no embedding endpoint | falls back to `region`; errors if that is `global` |
| `maas_region` | MaaS partners are regional-only; `global` 404s for them | falls back to `region`; MaaS dispatch fails with a fix-hint if that is `global` |
| `catalog_publishers` | Publisher catalogs probed for `GET /admin/api/models/available` | `["google", "anthropic"]` |
| `credentials_path` | Service-account key file; omit to use ADC (metadata server on GCE — no key material on disk) | ADC |
| `credential_source` | `"adc"` or `"metadata"` (see below). Contradicts `credentials_path` — setting both is a config-load error | `"adc"` |

**Vertex credentials.** Which Google credential the provider uses:

| `credential_source` | `credentials_path` | Credential |
|---|---|---|
| unset / `"adc"` | unset | Application Default Credentials: `GOOGLE_APPLICATION_CREDENTIALS`, then the gcloud user file (`~/.config/gcloud/application_default_credentials.json`), then the metadata server |
| `"metadata"` | unset | The GCE/GKE/Cloud Run metadata server's attached service account **only** — the gcloud user file and `GOOGLE_APPLICATION_CREDENTIALS` are ignored |
| unset | set | That service-account key file |
| `"adc"` / `"metadata"` | set | Rejected at config load (contradictory) |

ADC checks the gcloud user file *before* the metadata server, so on a VM meant to run as its
attached service account a single `gcloud auth application-default login` silently moves the
proxy onto a personal credential — and Google's reauthentication policy can later refuse to
refresh it (`invalid_grant` / `invalid_rapt`). Set `credential_source = "metadata"` on such
hosts. Under `"adc"`, when a refresh fails that way and the metadata server answers, the
provider switches to the metadata server for the rest of the process and logs a
`CREDENTIAL FALLBACK` warning.

When no fallback is available the failure is **permanent**, not transient — see
[Credential health and `credential_expired`](#credential-health-and-credential_expired),
which applies to every provider with a managed credential.


**The catalog lists only what the project can actually call.** The public publisher
catalog enumerates what *exists*; Model Garden partner (MaaS) models additionally gate on
per-project accepted terms. `GET /admin/api/models/available` therefore verifies access
per MaaS model with a minimal 1-max-token probe before listing (google/anthropic are
exempt — they are the structural dispatch arms, reachable whenever the provider works at
all): a 404/403 answer means the project lacks access and the model is **omitted**, with
an info log naming it. An invalid-body "free" probe does not work — Vertex validates the
request body before resolving model access, so it cannot distinguish the two cases.
Publishers the project cannot reach at all (catalog 403/timeout) are skipped with a
warning, never fatal: per-project Model Garden enablement variation is normal, not an
error. To make a listed-but-omitted model callable, accept the publisher's terms on its
Model Garden card in the GCP console — it appears in the catalog automatically afterward.

Runtime model aliases (`PUT /admin/api/aliases/{alias}`, stored in the DB) override
`routing.model_aliases` from config — this is the seam consumer apps use to let operators
repoint an abstract tier (`fast`/`balanced`/`deep`) at any catalog model without touching
config or redeploying. Fallback chains (`[routing.fallback_chains]`) accept any resolved
`provider/model` string, including cross-publisher Vertex chains
(`"vertex/anthropic/claude-…" = ["vertex/google/gemini-…"]`).

### Azure AI Foundry: one endpoint, many deployments

`[providers.foundry]` is to Azure what `[providers.vertex]` is to GCP. A Foundry
(AI Services) resource fronts every model deployed on it — Llama, Mistral, Phi, DeepSeek,
Cohere, Grok and the OpenAI models — behind one endpoint, with the deployment named in the
request body, so `foundry/<deployment>` is all a route needs. Chat, streaming, embeddings,
decisions (`POST /v1/decisions`), reranking (`POST /v1/rerank`) and `GET /admin/api/models/available` discovery all go
through it.

Distinct from `[providers.azure]`, which speaks the older Azure OpenAI surface
(`/openai/deployments/<deployment>/…?api-version=`) with an `api-key` header and needs one
provider entry per deployment. Use that for an Azure OpenAI resource, `foundry` for a
Foundry / AI Services resource.

| Key | Description | Default |
|-----|-------------|---------|
| `foundry_endpoint` | Resource endpoint, e.g. `https://<resource>.services.ai.azure.com` | required (`api_base` accepted) |
| `project` | Foundry project; appends `/api/projects/<project>` | none (resource-level) |
| `entra_scope` | Override the Entra audience | derived from the endpoint |
| `api_version` | Override the per-surface api-version | per surface (see below) |
| `decision_deployments` | Deployments that serve `POST /v1/decisions`. A bare `model` that is no alias and names no provider resolves against this list (a decision is never substituted by `routing.default_model`); a resolved deployment outside the list is refused with a 400 before a call | none (every resolved deployment passed through; a bare name no provider lists is still refused) |
| `account_resource_id` | The account's Azure resource id (`/subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.CognitiveServices/accounts/<name>`). When set, the catalog is read against the account's management-plane deployment list and keeps only names whose `provisioningState` is `Succeeded`; needs an Entra credential with read on the account (`Cognitive Services User`) | none (the configured `deployments` are the catalog) |
| `discover_deployments` | With `account_resource_id`: `deployments` is a seed, and every other `Succeeded` deployment on the account joins the catalog, classified from its capability map or name (`chat`, `embedding`, `rerank`, `image`, `audio`, `other`) and marked `source: discovered` on `GET /admin/api/models/available`; configured names carry `source: configured`; `decision_deployments` are never discovered as chat. A deployment added to the account appears at the next catalog read (`?refresh=true` forces one); one deleted disappears | `false` (the catalog is exactly the configured list) |
| `systemone_deployments` | Decision deployments the resource serves on its System One route (`{resource}/providers/microsoft/v1/systemone`) instead of `{base}/decisions`. A decision for one of these is re-shaped for that route both ways (see Decisions below); a deployment listed here is a decision deployment (no second entry under `decision_deployments` needed) | none (every deployment goes to `{base}/decisions`) |
| `rerank_deployments` | Deployments that serve `POST /v1/rerank`, by the same resolution rule: a bare `model` resolves against this list, a resolved deployment outside it is refused with a 400 before a call | none (every resolved deployment passed through; a bare name no provider lists is still refused) |
| `rerank_url` | Where rerank requests are posted. The rerankers Foundry serves speak the Cohere rerank API, which neither data plane below carries | the resource root + `/providers/cohere/v2/rerank`; set a serverless endpoint's own `https://<deployment>.<region>.models.ai.azure.com/v2/rerank` when deployed that way |

**The surface is the path you configure.** A bare host resolves to the OpenAI-compatible
data plane (`/openai/v1`), where `api-version` is optional and the service defaults to
`v1`. An endpoint ending in `/models` selects the Azure AI Model Inference data plane,
where `api-version` is *required* — the router supplies it. The Model Inference API is
documented as deprecated in favour of the OpenAI-compatible one, which is why a bare host
picks the latter. Discovery always reads the `/openai/v1/models` listing on the same
resource, because the Model Inference surface has no listing operation at all.

**Authentication is Microsoft Entra ID** — the same credential sources the `azure` and
`bing_grounding` providers use, sharing the same code; see
[Azure credentials](#azure-credentials). With no `credential_source`, Foundry reads the
environment (`AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET` for an app
registration, otherwise managed identity). Setting `api_key` with no `credential_source`
switches to resource-key auth (`api-key` header) — supported, but it puts a secret on disk.

The **audience** is derived per endpoint: `https://cognitiveservices.azure.com/.default`
for a resource endpoint (what the published OpenAPI documents declare),
`https://ai.azure.com/.default` for a project endpoint. Microsoft's keyless-auth how-to
shows `ai.azure.com` for resource endpoints too, so where spec and prose disagree
`entra_scope` is the override. The resolved value is logged at startup and named in any
401/403 response, so the fix is visible without a doc hunt.

### Azure credentials

> **Status: implemented from Microsoft's documentation; not yet exercised against a live
> Azure endpoint.** Every source below is covered by fake-server tests of the documented
> request and response shapes. Run the [live-test checklist](#azure-live-test-checklist)
> before relying on a source in production, and report anything that differs.

`credential_source` selects a Microsoft Entra ID credential for the three Azure-facing
providers: `[providers.azure]`, `[providers.foundry]` and `[providers.bing_grounding]`. The
same key on `[providers.vertex]` takes the Google values above; any other value, or the key on
any other provider, is a config-load error that names the providers which accept it.

| `credential_source` | Credential | `kind` in health |
|---|---|---|
| `"managed-identity"` | The platform's managed identity. App Service / Container Apps / Functions style when `IDENTITY_ENDPOINT` and `IDENTITY_HEADER` are set (`api-version=2019-08-01`, `X-IDENTITY-HEADER`); otherwise the VM/AKS instance metadata service at `169.254.169.254` (`api-version=2018-02-01`, `Metadata: true`). `azure_client_id` selects a user-assigned identity | `azure-managed-identity` |
| `"workload-identity"` | Federated token file (AKS workload identity, other OIDC federation) exchanged at the tenant's token endpoint as a `client_credentials` grant with a `jwt-bearer` client assertion. The file is re-read on every exchange, because the platform rotates it | `azure-workload-identity` |
| `"client-secret"` | App registration client secret, `client_credentials` grant | `azure-client-secret` |
| `"cli"` | `az account get-access-token --resource <audience>` — a developer's personal login | `azure-cli` |
| `"default"` | The first source that resolves, in this order: client secret (tenant + client id + secret all set) → workload identity (federated token file set) → App Service managed identity → instance metadata service (a 2s probe) → Azure CLI | `azure-default-<resolved>`, e.g. `azure-default-managed-identity` |
| unset | `azure`: resource key (`api_key`), as before. `foundry` / `bing_grounding`: key auth when `api_key` is set, otherwise the environment — client secret if the `AZURE_*` trio is set, else managed identity | `azure-client-secret` / `azure-managed-identity` |

Setting both `api_key` and `credential_source` is rejected as contradictory. For `azure`,
the audience is `entra_scope`, defaulting to `https://cognitiveservices.azure.com/.default`.

| Key | Environment fallback | Used by |
|---|---|---|
| `azure_tenant_id` | `AZURE_TENANT_ID` | `client-secret`, `workload-identity`, `cli` (`--tenant`) |
| `azure_client_id` | `AZURE_CLIENT_ID` | `client-secret`, `workload-identity`, `managed-identity` (user-assigned) |
| `azure_client_secret` | `AZURE_CLIENT_SECRET` | `client-secret` |
| `azure_federated_token_file` | `AZURE_FEDERATED_TOKEN_FILE` | `workload-identity` |
| `entra_scope` | — | audience override |

A config value wins over its environment variable. `AZURE_AUTHORITY_HOST` overrides
`https://login.microsoftonline.com` (sovereign clouds); `AZURE_POD_IDENTITY_AUTHORITY_HOST`
overrides the metadata-service host. A secret-bearing key is only accepted next to a
`credential_source` that reads it, and a missing required setting fails the first token
request with an error naming each missing key.

**Token lifetime.** `expires_in` (seconds) and `expires_on` (epoch seconds, as a number or
a string; the CLI's local-time `expiresOn` too) are both honoured. Tokens are refreshed
early, five minutes before expiry or at half their lifetime, whichever is sooner. A response
that states no lifetime is cached for five minutes.

**Which failures are permanent.** A token-endpoint answer is classified as needing
reauthentication, and so becomes `credential_expired`, when it carries OAuth2 error
`invalid_grant` or `interaction_required`, or one of these AADSTS codes: `50173` (grant
revoked by a password change), `70043` / `700082` (refresh token expired by sign-in
frequency or inactivity), `50076` / `50079` / `50078` (MFA or re-enrolment required),
`700024` (client assertion outside its validity window: the federated token went stale),
`7000222` / `7000215` (client secret expired or invalid). A CLI failure that asks for
`az login` or carries an AADSTS reauth code is classified the same way. Everything else,
including 5xx, 429 and network failures, stays transient and goes through normal retry and
circuit-breaker handling.

**Fallback.** Under `"default"`, when the chain resolved to the Azure CLI and that login
dies, the router probes the managed-identity endpoint once. If it answers, the provider
switches to it for the rest of the process and logs `CREDENTIAL FALLBACK`. No other Azure
source has a fallback: an explicit choice is honoured, not second-guessed.

#### Azure live-test checklist

For each source: add the snippet, start the router on a host where that source exists, and
send the proving request. Every source passes when **(a)** the request succeeds and **(b)**
`GET /health/deep` lists the provider under `credentials` with the expected `kind` and
`status`. Replace `<resource>`/`<deployment>` with your own; values in angle brackets are
placeholders.

The proving request is a chat completion through the configured provider:

```bash
curl -s localhost:8090/v1/chat/completions -H "Authorization: Bearer $ROUTER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"foundry/<deployment>","messages":[{"role":"user","content":"ping"}],"max_tokens":5}'
curl -s localhost:8090/health/deep | jq '.credentials'
```

1. **Managed identity (VM / AKS instance metadata)** — expect `azure-managed-identity`, `ok`.
   The identity needs the *Cognitive Services User* role (or equivalent) on the resource.
   ```toml
   [providers.foundry]
   foundry_endpoint = "https://<resource>.services.ai.azure.com"
   credential_source = "managed-identity"
   # azure_client_id = "<user-assigned identity client id>"
   ```
2. **Managed identity (App Service / Container Apps)** — the same snippet, on a host where
   `IDENTITY_ENDPOINT` and `IDENTITY_HEADER` are set. Expect `azure-managed-identity`, `ok`.
3. **Workload identity (AKS)** — expect `azure-workload-identity`, `ok`. The pod needs the
   workload-identity label; the webhook injects `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and
   `AZURE_FEDERATED_TOKEN_FILE`.
   ```toml
   [providers.foundry]
   foundry_endpoint = "https://<resource>.services.ai.azure.com"
   credential_source = "workload-identity"
   ```
   Also run for longer than the projected token's lifetime (about an hour) to prove that the
   file is re-read.
4. **Client secret** — expect `azure-client-secret`, `warn`. Put the secret in
   `AZURE_CLIENT_SECRET` rather than the file where you can.
   ```toml
   [providers.azure]
   api_base = "https://<resource>.openai.azure.com/openai/deployments/<deployment>"
   credential_source = "client-secret"
   azure_tenant_id = "<tenant id>"
   azure_client_id = "<app client id>"
   ```
   Also prove the error path: set a wrong secret and expect HTTP 401,
   `error.code = "credential_expired"` (AADSTS7000215). Prove this one with
   `"model":"azure/<model>"` in the chat request.
5. **Azure CLI** — expect `azure-cli`, `warn`, after `az login` as the user running the router.
   ```toml
   [providers.foundry]
   foundry_endpoint = "https://<resource>.services.ai.azure.com"
   credential_source = "cli"
   ```
   Also prove the error path: run `az logout` and restart, then expect HTTP 401
   `credential_expired` whose `remediation` names managed identity.
6. **Default chain** — `credential_source = "default"` on each of the hosts above. Expect
   `azure-default-<source>` naming the source that host provides. On a VM with a managed
   identity *and* an `az login`, with no `AZURE_*` variables set, expect
   `azure-default-managed-identity` (the metadata service comes before the CLI).
7. **Bing grounding** — repeat source 1 or 3 on `[providers.bing_grounding]`, then prove it
   with a search: `curl -s localhost:8090/v1/search -H "Authorization: Bearer $ROUTER_KEY"
   -H 'Content-Type: application/json' -d '{"engine":"bing_grounding","query":"ping"}'`.

### Credential health and `credential_expired`

Every provider with a managed credential (`vertex`, `azure`, `foundry`, `bing_grounding`)
shares one policy, implemented once in the core `CredentialChain`.

**A dead credential is permanent, not transient.** When a token refresh fails with a
reauthentication-class error and no fallback answers, the request fails with HTTP **401**:

```json
{"error": {"code": "credential_expired", "type": "credential_expired",
  "message": "...", "provider": "foundry", "credential_kind": "azure-cli",
  "reason": "the Azure CLI login can no longer be refreshed without interactive sign-in",
  "remediation": "Run az login, or set credential_source = \"managed-identity\" under [providers.foundry] ..."}}
```

It is not retried and does not count toward the circuit breaker. Retrying cannot repair a
credential, and an open breaker would hide the actionable error behind a "circuit breaker
open" that invites retries. The dead state is cached for 60 seconds, so a burst of requests
does not hammer the token endpoint. After that the chain probes again: a repaired login
recovers without a restart. Fallback probes, where a source has one, are spaced the same way.

**Verdicts.** `GET /health/deep` reports each credential without ever including the credential
itself: under `capabilities.<llm|embedding|search>.credential` for the probed adapter, and in a
top-level `credentials` array listing **every** configured provider with a managed credential.
Each entry has:

| Field | Meaning |
|---|---|
| `status` | `ok` (a workload identity that renews itself), `warn` (works now, but will expire or is a secret at rest), `expired` (dead; requests are failing with `credential_expired`), `unknown` (not resolved yet, or a type the router does not classify) |
| `reason` | Why, in one line |
| `remediation` | What to change; empty when `ok` |
| `source` | The configured `credential_source` (`environment` when unset) |
| `kind` | The credential actually in use |
| `fallback_active` | `true` once a fallback has taken over |

`credentials` entries also carry `provider` and `capability`. A provider on key auth is not
listed: there is no credential lifecycle to report.

| Kind | Status |
|---|---|
| `metadata`, `explicit-file`, `adc-service-account` | `ok` |
| `adc-user` (a `gcloud auth application-default login` file) | `warn` |
| `adc-other` | `unknown` |
| `azure-managed-identity`, `azure-workload-identity` | `ok` |
| `azure-client-secret`, `azure-cli` | `warn` |
| `azure-default-unresolved` (no request yet) | `unknown` |
| any kind whose refresh has failed permanently | `expired` |

**Build note:** default cargo features include every provider (`vertex`, `bedrock`, `foundry`, …).
An unconfigured provider costs nothing at runtime, while a binary missing a compiled
feature *refuses to start* against a config that names it — so feature-stripped builds
turn a plain `cargo build` into a startup trap. Only strip features deliberately.

### Failure capture

A `prompts` row is only ever written on the success path. Everything that failed —
a model that resolved to a provider with no adapter, a budget denial, an upstream
error — left no trace in the router at all, so diagnosing it meant reading the
*calling application's* logs. That is backwards: the router is the one component
that sees every call from every app.

Every failed request is now persisted to `request_failures` and shown at
`/admin/failures`, with counts grouped by **stage**:

| Stage | Meaning | Whose problem |
|---|---|---|
| `resolve` | model/provider resolution — unknown provider, no configured adapter | yours (config or caller) |
| `policy` | budget, rate limit, guardrail or policy denial | expected, or a limit to raise |
| `provider` | the upstream was reached and returned an error | the upstream's |
| `request` | the caller's request was malformed or unacceptable | the caller's |
| `internal` | anything else raised inside the router | a bug |

The `resolve`/`provider` split is the point. A resolve failure is always
actionable by the operator, and lumping it in with ordinary upstream flakiness is
how one deployment let 196 `Unknown provider: anthropic` errors pass as noise for
a full run.

Capture wraps each handler as a whole rather than sitting at individual error
returns, so an error path added later cannot silently escape being recorded. No
prompt or response bodies are stored — a failure record must never become a
second, unlogged copy of content that `X-No-Log: true` was used to suppress.

### Running on PostgreSQL

SQLite is the default. To use PostgreSQL 14+, build with `--features postgres` and set
the connection URL:

```toml
[database]
postgres_url = "postgres://modelrouter:secret@db-host:5432/modelrouter"
```

or `MODELROUTER_DATABASE__POSTGRES_URL=postgres://…` (an empty value switches it back
off). `serve`, `migrate`, and the `user`, `admin`, `key`, `budget`, `group`, `model`,
`alias`, `provider`, `webhook`, `experiment`, `report` and `audit` commands all use it, and each one applies
the Postgres migrations (`migrations/postgres/`) before running. A binary built without
the feature refuses to start when `postgres_url` is set, rather than quietly using SQLite.

Every CLI command, report and dashboard page works on either backend. A dedicated
prompt-log file (`storage.prompt_db_path`) is always SQLite.

**Docker Compose.** `docker-compose.postgres.yml` is an overlay that adds a `postgres:17`
service (data in the `pgdata` volume, published on `127.0.0.1:5438`) and builds
modelrouter with `FEATURES="otel,postgres"`. With the OTel stack:

```bash
echo "MODELROUTER_PG_PASSWORD=$(openssl rand -hex 24)" > .env    # gitignored
docker compose -p modelrouter -f docker-compose.otel.yml -f docker-compose.postgres.yml up -d --build
docker compose -p modelrouter -f docker-compose.otel.yml -f docker-compose.postgres.yml \
  exec -it modelrouter /modelrouter admin create --name ops --role superadmin
```

Every service has `restart: unless-stopped`, so the stack comes back after a reboot as
long as Docker itself starts at login (Docker Desktop: *Settings → General → Start Docker
Desktop when you sign in*).

**Testing against Postgres.** `tests/test_db_conformance.rs` runs every repository
scenario against SQLite and, when `MODELROUTER_TEST_POSTGRES_URL` is set, against a fresh
Postgres schema per test:

```bash
MODELROUTER_TEST_POSTGRES_URL=postgres://modelrouter:$MODELROUTER_PG_PASSWORD@127.0.0.1:5438/modelrouter_test \
  cargo test --features postgres --test test_db_conformance
```

### Subscription mode: chat through the vendor CLIs

With `[subscription]` enabled, chat traffic for Anthropic, OpenAI and Gemini
models is answered by the vendor's own CLI in headless mode, on that CLI's
subscription login, instead of the HTTP API. The router still authenticates,
applies policy and budgets, and meters every call, so subscription usage is
tracked per user, project and model just like API usage.

```toml
[subscription]
enabled = true
providers = ["anthropic", "openai", "gemini"]   # which providers to redirect
```

| Provider | CLI | Metered as |
|---|---|---|
| `anthropic` | `claude -p --output-format json` | `claude-cli` |
| `openai` | `codex exec --json` (read-only sandbox) | `codex-cli` |
| `gemini` | `gemini --output-format json` | `gemini-cli` |

It covers `/v1/chat/completions` (streamed or not, plus fallback chains and
load-balancer pools), `/v1/responses` and `/v1/messages`. Embeddings, images and
search keep using the APIs.

**Sessions.** Each CLI keeps a conversation it can resume. The router remembers
which session holds which transcript. When a request's messages continue a
conversation the router has answered, it resumes that session and sends only the
new user turn, so the CLI's prompt cache keeps working. A first turn, an edited
history or a restart starts a new session and replays the transcript as text.
Sessions are kept per user, API-key project, CLI and model. Each user/project
gets its own working directory under `work_dir`, and a session is forgotten after
`session_idle_secs` without use. Two concurrent requests never share a session.

**Accounting.** Ledger rows carry the CLI's provider name with the token counts
the CLI reports, priced at the model's API rate. Cost reports therefore show
what the subscription usage would have cost through the API; budgets count it
the same way. A model without a pricing entry is recorded at $0.

**Limits.**
- Text only: tool definitions, tool calls, images and documents are refused with
  a 400 (the CLIs run with their own tools disabled or sandboxed).
- `temperature`, `max_tokens` and `reasoning_effort` are not passed to the CLIs.
- A streamed request is answered in one piece once the CLI finishes.
- Each CLI adds its own instructions to the prompt: Claude is given the caller's
  system prompt (or a neutral one) in place of Claude Code's; Codex keeps its
  built-in agent instructions, about 18k input tokens per new session.
- The vendor's API-key variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `GEMINI_API_KEY`, …) are removed from the CLI's environment, so calls cannot
  fall back to API billing.
- Which models work depends on the subscription: Codex on a ChatGPT login, for
  example, refuses models that plan does not include, and the router returns
  the CLI's message.

**Where it can run.** The CLIs must be installed and logged in as the user
modelrouter runs as. That makes it a host deployment (`modelrouter serve` or
`install-service`): the Docker images contain no CLIs or logins, and Claude
Code's macOS login lives in the keychain.

A host deployment can still keep Postgres and the OpenTelemetry stack in Docker:

```bash
# Supporting services only — leave the modelrouter container out.
docker compose -p modelrouter -f docker-compose.otel.yml -f docker-compose.postgres.yml \
  up -d postgres otel-collector phoenix prometheus

cargo install --path . --features otel,postgres --locked   # → ~/.cargo/bin/modelrouter
modelrouter init        # then set in ~/.modelrouter/config.toml:
#   [database]  postgres_url = "postgres://modelrouter:<password>@127.0.0.1:5438/modelrouter"
#   [telemetry] enabled = true, endpoint = "http://localhost:4317"
#   [subscription] enabled = true
modelrouter install-service   # launchd agent: starts at login, restarts on exit
```

`install-service` writes `~/Library/LaunchAgents/dev.modelrouter.plist` with this
binary's path, the config `--config`/`MODELROUTER_CONFIG` selects, and the
installing shell's `PATH` (so the agent finds the CLIs); it logs to
`modelrouter.log` beside the config. Until Docker has Postgres up after a reboot,
the agent exits and launchd retries it every ten seconds.

### Prompt-log storage

The `[storage]` section controls what the prompt log records. Defaults are
privacy-first: rows are written, but **message/response content is not stored**
unless explicitly enabled, and nothing is ever purged unless a retention is
configured.

```toml
[storage]
store_prompts = true          # false skips prompt-log rows entirely
store_prompt_content = false  # true stores full request/response bodies
prompt_retention_days = 0     # purge rows older than N days; 0 = keep forever
locked = false                # true pins this policy to the file (see below)
```

- `store_prompts = false` skips the prompt-log INSERT on every route. Cost
  tracking (`cost_ledger`) is unaffected — budgets and the cost dashboard keep
  working, with `prompt_id` left NULL.
- `store_prompt_content = false` (the default) stores metadata only — tokens,
  cost, model, timestamps — with a placeholder in place of message bodies.
  The same policy applies to the observability egress: a Langfuse, LangSmith
  or webhook backend receives the placeholder too, never the bodies.
  The per-request `X-No-Log: true` header still suppresses the row entirely.
- `prompt_retention_days > 0` deletes older rows on an hourly check. Deletion
  is strictly opt-in: the default keeps everything forever.
- `prompt_db_path = "~/.modelrouter/prompts.db"` (optional) puts the prompt log
  in its own SQLite file so it can be rotated/archived independently of the
  main DB. Restart-scoped (not editable in the GUI); SQLite only — the
  Postgres backend ignores it. Cost rows stay in the main DB.

These settings can also be edited in the admin dashboard on the **Prompts**
page; values saved there persist in the database, override `config.toml`, and
apply immediately without a restart.

`locked = true` (config file only) pins the policy to `config.toml`, for a
deployment that must never keep conversation bodies:

- the dashboard shows the settings read-only and refuses a save;
- a policy saved from the dashboard earlier is ignored at start-up, with a warning;
- while `store_prompt_content = false`, no request stores content: creating an
  experiment with `retain_content: true` is refused (API, dashboard and CLI),
  and an existing retaining experiment's requests are stored as metadata only.

### Retention for the other per-request tables, and vacuum

`prompt_retention_days` bounds the prompt log only. Four more tables grow with
traffic and are kept forever by default: `cost_ledger` (one usage row per
request, the source for budgets, the cost pages, comparisons and experiment
results), `request_failures`, `hook_metrics` and `audit_log` (one row per
admin action, so it grows slowly). The `[retention]` section gives each a
retention, in days, applied on the same hourly tick as the prompt log. `0`
(the default) keeps that table forever: deletion is opt-in, as everywhere.

```toml
[retention]
cost_ledger_days = 90        # usage rows; 0 = keep forever
request_failures_days = 30   # failed-request records
hook_metrics_days = 30       # per-invocation hook timings
audit_log_days = 0           # admin audit trail: kept
```

- Config file only (restart-scoped): there is no dashboard form, so the value
  in the file is the value in effect.
- **A retention never makes a budget under-count.** The usage-row cutoff is
  held at the earliest moment any budget still reads from: never inside the
  current calendar month (daily, weekly and monthly budgets and reports read
  it, whenever they are created), and never past the `window_start` of a
  `total` budget rule. A `total` rule with no start reads from the epoch and
  holds the whole ledger; the log says which rule, and deleting it or giving
  it a start lets retention run. The same clamp applies to `policy_rules`
  budgets in the config, whose windows are at most monthly.
- Reports and experiment results read what is left: an experiment older than
  `cost_ledger_days` has no per-run cost rows any more. Set it at least as
  long as the oldest report you want to be able to run.
- The purge of the prompt log detaches usage rows from the prompt rows it
  deletes (`cost_ledger.prompt_id` becomes `NULL`); the rows themselves follow
  `cost_ledger_days`.

**Reclaiming the space.** Deleting rows frees pages inside the SQLite file,
which new rows reuse, so the file stops growing once retention is in effect;
it does not shrink by itself. A database file created by this version is in
incremental auto-vacuum mode, and after every purge the tick returns the free
pages to the file system (`PRAGMA incremental_vacuum`, cheap at any size). A
file created earlier keeps its mode until it is converted once:

```bash
modelrouter db vacuum
```

That is a full `VACUUM`: it rewrites the file (free disk of about the file's
size is needed) and holds the write lock while it runs, so run it in a quiet
window; writers wait and retry rather than fail for short runs. The command
prints the size and free pages before and after and the mode it left the
file in; from then on the hourly tick keeps the file trimmed. Until it is
run, the tick logs the free page count with that hint. On Postgres the
purges are the same and autovacuum reclaims the space; the command does
nothing there.

### Response cache

The router can serve identical requests from a cache instead of calling the
provider, so every app behind it saves money without implementing its own cache.
Disabled by default; enable with `cache.enabled = true`.

**What is cached.** `POST /v1/chat/completions`, `POST /v1/messages` and
`POST /v1/search`, streamed or not. `POST /v1/responses` has no streaming mode
and is not cached.

**Streaming.** A streamed completion that finishes cleanly (the provider sent
its terminal event and usage) is assembled and stored under the same key as the
equivalent plain request. A stream that errors, is cut off by the client, or
carries anything the assembler does not understand is not stored. On a hit for
a `stream: true` request the entry is replayed as SSE in the wire format of the
endpoint that was called: `chat.completion.chunk` events, the router's
usage-and-cost chunk and `data: [DONE]` for chat completions, or the
`message_start` … `message_stop` event sequence with final usage for
`/v1/messages`. A plain request can hit an entry recorded from a stream and the
other way round. Clients that rerun identical workloads get the same answer
whichever mode they use.

**Cache key.** The resolved (post-alias, post-load-balancer) model, plus a
SHA-256 of the request body with only transport-level fields removed (`stream`,
`stream_options`, `user`, `session_id`, `metadata`, `attribution`). Every field
that can change the answer — messages, tools, `temperature`, `top_p`,
`max_tokens`, `seed`, `response_format` — is part of the key. For search the key
is engine + query + options. Nothing else goes in: who is asking and the
`x-modelrouter-cache*` request headers never change the key, so rerunning the
same prompts or queries hits the cache. Exact-match only; there is no semantic
or fuzzy matching.

**Eligibility (the default is conservative).** A completion is cached only when
its `temperature` is explicitly at or below `cache.completions.max_temperature`,
which defaults to `0.0`. A request that omits `temperature` is scored using
`cache.completions.assumed_temperature` (default `1.0`, the OpenAI default), so
**omitting `temperature` means the request is not cached**. Search queries are cached by default with a shorter TTL
(15 minutes vs. 1 hour). Raise `max_temperature` deliberately if you want
creative sampling replayed.

**Per-request control.** A caller can steer the cache for one request with the
`x-modelrouter-cache` request header, on `/v1/chat/completions`,
`/v1/messages` and `/v1/search`:

| Value | Effect | Response header |
|---|---|---|
| (absent) | The eligibility rules above decide | `HIT` / `MISS`, or none when not cached |
| `use` | Look up and store even when the request is not eligible by default, e.g. a sampled `temperature` | `HIT` / `MISS` |
| `bypass` | Neither look up nor store | `BYPASS` |
| `refresh` | Skip the lookup, call the provider, store the fresh answer over the old entry | `REFRESH` |

The response header is a status indicator, not an error. `MISS` means the
cache was consulted and held no entry; a response the cache was not involved
in (cache off, request ineligible, or not opted in under `require_opt_in`)
carries no `x-modelrouter-cache` header at all.

Values are case-insensitive; anything else is a 400. The header never switches
on a cache or class the operator has disabled, and it never overrides a
per-group `cache_enabled = false` or an experiment binding. `use` and `refresh`
widen what is cached, so an operator can refuse them with
`cache.allow_header_opt_in = false`; the router then treats them as absent.
`bypass` only narrows and is always honoured. `use` suits clients that rerun
identical workloads with sampling on and want the first answer replayed.

**Opt-in only.** `cache.require_opt_in = true` turns the default around: a
request is cached only when it carries `x-modelrouter-cache: use` (lookup and
store) or `refresh` (store only). A request without the header is neither
looked up nor stored, however eligible it would otherwise be, and it never
reads an entry a `use` request stored. Use it when the client decides
call by call what may be cached, and a call that forgets the header must not
be. `cache stats` and the stats API report `require_opt_in` and
`skipped_without_opt_in`, the number of header-less requests it skipped. A
rising count names a client that is not opting in. It requires
`allow_header_opt_in = true`; the contradictory pair is refused at start-up.

**Per-request TTL.** `x-modelrouter-cache-ttl: <seconds>` sets how long the
entry this request stores will live, in place of the class TTL; `0` asks for an
entry that never expires. The value is capped by `cache.max_ttl_seconds`
(default `86400`, one day), so with the default a request for `0` or for a week
is stored for a day. Setting `max_ttl_seconds = 0` lifts the cap and lets
callers store entries with no expiry. The header only affects what this
request stores: it does not change the lifetime of an entry it hits, and it is
ignored under `bypass`. A value that is not a whole number of seconds, or is
longer than ten years, is a 400. It applies to all three cached endpoints and
combines with `use` and `refresh`, e.g. `refresh` plus a TTL re-records an
answer with a new lifetime.

Entries without an expiry stay until they are purged or evicted. On the
`memory` backend that means the `max_entries` bound, and they vanish on
restart. On `redis` they are written without `EX`, so they survive only as
long as Redis keeps its data. If you allow unlimited entries, run Redis with
persistence (AOF or RDB snapshots) so a restart does not drop them, and set a
`maxmemory` limit with an eviction policy that fits (`allkeys-lru` evicts any
key under memory pressure; `volatile-lru` evicts only keys with a TTL, so
unlimited entries are never evicted and Redis refuses writes once full).

**Namespaces.** `x-modelrouter-cache-namespace: <name>` labels the entries a
request stores and the hits and misses it scores. The name is 1-64 characters
from `A-Z a-z 0-9 . _ -`; anything else is a 400. The label is not part of the
cache key: the same prompt or query hits the same entry whichever namespace
(or none) asks, so a rerun is served from the cache no matter how it is
labelled. It applies to all three cached endpoints and combines with the mode
and TTL headers. A test suite can label its traffic, watch its hit rate, give
its entries their own lifetime, and purge what it recorded:

```bash
modelrouter cache purge --namespace nightly-eval --admin alice
# or: POST /admin/api/cache/purge {"scope":"namespace","namespace":"nightly-eval"}
```

Purge removes the entries that requests in that namespace stored, including
ones other callers have since been hitting. An entry keeps the label of the
request that stored it; a later hit from another namespace does not relabel
it, while a `refresh` does. On Redis the label is inside the stored value, so a
namespace purge reads every entry under the key prefix.

An operator can give a namespace its own default TTL:

```toml
[cache.namespaces.nightly-eval]
ttl_seconds = 0        # 0 = never expire; otherwise seconds
```

A caller's `x-modelrouter-cache-ttl` still wins, within `max_ttl_seconds`. The
namespace default is operator config, so it is not capped by
`max_ttl_seconds`; the Redis persistence advice above applies to it too. A
namespace without an entry uses the class TTL.

`cache stats` and `GET /admin/api/cache/stats` report hits, misses, stores and
savings per namespace under `live.by_namespace`, for this process. A hit counts
for the namespace that asked, not the one that stored the entry. The first
256 namespaces seen are tracked individually and the rest are counted together
under `*`. Default-namespace traffic appears only in the totals.

**Metering.** A cache hit is recorded as a usage row with `cache_hit = true`,
`cost_usd = 0`, and the avoided provider cost in `saved_usd`. Hits therefore
never inflate spend, and cache-hit percentage is reported alongside cost.

**Uncached cost.** A rerun of an identical workload is mostly served from the
cache, so its paid cost says more about the order runs happened in than about
the workload: whichever run went last looks cheapest. Cost views therefore
list an uncached cost, `cost_usd + saved_usd`, next to the paid cost. It prices
every request as if the cache were empty, so reruns of the same work land on
the same figure. It appears as `uncached_cost_usd` in response `usage` and
`x_router.cost`, in `/admin/api/compare` (with `uncached_cost_per_request` and
deltas), experiment results, attributed usage, and the `report cost`,
`report usage`, `report compare` and `experiment results` CLI output, and as an
"Uncached" column on the dashboard's cost, reports, compare, attribution and
experiment pages. Budgets, budget burndown and the overview page stay on
paid cost. Only the router's response cache is undone: discounts the provider
applies for its own prompt cache are already in `cost_usd` and remain there.
Responses carry an `x-modelrouter-cache: HIT|MISS|BYPASS|REFRESH` header,
streamed replays included, and the `response cache hit` log line records `streamed=true|false`.

**Backends.** `memory` (moka, bounded by `cache.max_entries`, per-process — each
replica warms its own copy) or `redis` (shared across stateless replicas, keys
prefixed by `cache.namespace`; that setting is the Redis key prefix, unrelated
to the per-request namespace header). Backends sit behind the `CacheStore` trait in
`src/router/cache/store.rs`; adding another one touches no call sites.

**Operator surfaces.**

```bash
# CLI (talks to the running router's admin API)
modelrouter cache stats --admin alice
modelrouter cache purge --model gpt-4o-mini --admin alice
modelrouter cache purge --namespace nightly-eval --admin alice
modelrouter cache policy get --admin alice
modelrouter cache policy set --max-temperature 0.2 --admin alice

# REST (admin JWT)
GET  /admin/api/cache/stats
POST /admin/api/cache/purge     {"scope":"all"|"model"|"namespace"|"key", "model":…, "namespace":…, "key":…}
GET  /admin/api/cache/policy
PUT  /admin/api/cache/policy    {"completions_max_temperature": 0.2, …}
```

The dashboard page at `/admin/cache` shows hit rate over time, cache size, top
cached models, per-namespace counters, purge controls, and the policy form; `/admin/cost` shows cache-hit
% and dollars saved next to spend. Policy changes made at runtime are **not**
written back to the config file — a restart returns to the configured policy.

### Model routing

Models resolve in this order:

0. For a request bound to an experiment, its variant's overlay; then, for a request whose
   attribution tags carry a scope's tag, that scope's override (see *Scoped alias overrides*)
1. Runtime aliases (`model_aliases` table — admin API / dashboard / CLI)
2. Aliases attached to enabled registered models (the `Alias` column on `/admin/models`)
3. Alias lookup from `routing.model_aliases` in `config.toml`
4. Provider prefix — `anthropic/claude-opus-4-6` routes to the `anthropic` provider
5. Fall back to `routing.default_provider`

Alias chains are followed up to 10 hops; anything longer (or circular) falls through to the
default model rather than looping.

#### Runtime model aliases

Config-file aliases are the bootstrap set. To change the environment-wide alias map without
editing files or restarting, use the runtime alias surfaces — every write persists to the
database, updates the live router immediately, and is recorded in the audit log.

```bash
# CLI (writes to the database; a running server picks it up on restart)
modelrouter alias list
modelrouter alias set deep anthropic/claude-opus-4-6
modelrouter alias rm deep
```

```bash
# Admin API (takes effect on the next request, no restart)
curl -X PUT http://localhost:8080/admin/api/aliases/deep \
  -H "Authorization: Bearer $ADMIN_JWT" \
  -H 'Content-Type: application/json' \
  -d '{"target":"anthropic/claude-opus-4-6"}'

curl -H "Authorization: Bearer $ADMIN_JWT" http://localhost:8080/admin/api/aliases
curl -X DELETE -H "Authorization: Bearer $ADMIN_JWT" \
  http://localhost:8080/admin/api/aliases/deep
```

Reads require any admin role; writes require `superadmin`. The dashboard equivalent is the
**Model Aliases** section of `/admin/models`. A target may be a concrete `provider/model`
or another alias; writes that would create a cycle are rejected with a 400.

#### Scoped alias overrides

A scoped override maps aliases to models for requests carrying one attribution tag, so one
tenant, project or run can be routed to different models while every caller keeps sending the
same alias names. A scope is a tag `key=value`; writing a scope replaces its whole set of
aliases. Targets are pinned when written through the same gate as experiment variants (no
pool, no default-model substitution, a configured provider, a pricing entry), so a later edit of
a global alias never changes an override. A pinned request skips the complexity downgrade, load
balancer, session affinity, response cache and fallback, like an experiment-bound one; the
ledger records the model that answered and the prompt row keeps the alias that was asked for.

```bash
curl -X PUT http://localhost:8080/admin/api/scoped-aliases/tenant/acme \
  -H "Authorization: Bearer $ADMIN_JWT" -H 'Content-Type: application/json' \
  -d '{"aliases":{"deep":"anthropic/claude-opus-4-6","fast":"openai/gpt-5-mini"},"expires_at":"2030-01-01T00:00:00Z"}'

curl -H "Authorization: Bearer $ADMIN_JWT" "http://localhost:8080/admin/api/scoped-aliases?tag_key=tenant"
curl -X DELETE -H "Authorization: Bearer $ADMIN_JWT" http://localhost:8080/admin/api/scoped-aliases/tenant/acme
```

`expires_at` is an RFC3339 time in the future or `0` for never. An expired override stops
applying at once; a minute-by-minute tick then deletes it (audited as `system`) and reloads both
the overrides and the global runtime aliases, so replicas sharing a database converge. When a
request's tags name several scopes that map the requested alias, the smallest tag key wins.
Reads require any admin role; writes require `superadmin` and are audited.

An alias may also set request parameters, by writing `{"target": ..., "params": {...}}` in
place of the bare target:

```bash
-d '{"aliases":{"deep":{"target":"foundry/o3","params":{"reasoning_effort":"high","max_tokens":32000}}},"expires_at":0}'
```

The parameters are checked against the pinned model's parameter schema when written. An
unsupported parameter, or a value out of range, is refused with a 400 that names the model and
the parameter. On `/v1/chat/completions` they overwrite the caller's values for every request
the scope routes. The other surfaces only route the model.

Each model's schema appears as `parameters` in `GET /admin/api/models/available`.
`GET /admin/api/models/parameters?target=<alias or provider/model>` returns the schema of
whatever a target resolves to now. The schema lists only what the router actually sends for
that model:

- `max_tokens`, up to the model's output ceiling where it is known.
- `temperature`, unless the model rejects it.
- `reasoning_effort`, only where the provider adapter forwards a reasoning control. It lists
  only the levels the model accepts as they are. Claude 5 models take `low` to `max`, plus `none`
  where thinking can be turned off. OpenAI reasoning models (o-series, GPT-5) take their own
  levels; on Foundry the router forwards `reasoning_effort` to them.

#### Strict model resolution

Step 3 is a **substitution**: the caller asked for one model and a different one
answers. That is convenient for a shortcut and dangerous for anything else — a
name that is neither an alias nor `provider/model` is silently served by
`default_model`, and the response looks like an ordinary success. In one observed
deployment 1,330 requests for `claude-opus-4-5-20251101` were answered by
`gpt-4o-mini` before anyone noticed.

```toml
[routing]
strict_model_resolution = true   # default: false
```

With it **on**, an unresolvable model is refused with a message naming both models
and the ways to fix it:

```
model 'claude-opus-4-5-20251101' is not a configured alias and names no provider;
refusing to substitute 'gpt-4o-mini' for it. Add it to [routing.model_aliases],
request it as 'provider/model', or disable routing.strict_model_resolution.
```

With it **off** (the default, so existing deployments are unaffected) the
substitution still happens, but is logged at WARN with both names rather than
passing unremarked.

Turning it on pairs naturally with addressing the router by **abstract tier** —
callers send `fast`/`balanced`/`deep` and which model serves each tier stays an
operations decision in config, instead of a literal compiled into an application
that breaks the day the provider lineup changes.

#### Fallback chains

Keys are **canonical** model names (post-alias-resolution, no provider prefix);
values are re-resolved, so they carry one. Chains apply to both
`/v1/chat/completions` and `/v1/embeddings`.

```toml
[routing.fallback_chains]
"gpt-4o-mini" = ["ollama/qwen3.5:4b"]
"nomic-embed-text:latest" = ["openai/text-embedding-3-small"]
```

A chain is walked when the provider call fails **or when the provider has no
configured adapter** — the most common way a request dies, and exactly the case an
alternative fixes. The walk is bounded at 8 hops, so a chain that points back at
itself errors instead of spinning; every hop is a real provider call, so avoid
cycles rather than relying on the bound.

#### Routing Shortcuts

Use `:fastest` or `:cheapest` as the `model` value in any request to route to your configured fastest or cheapest target without changing client code.

Configure targets in `config.toml`:

```toml
[routing.shortcuts]
fastest  = "anthropic/claude-haiku-4-5"   # low-latency model
cheapest = "deepseek/deepseek-chat"        # lowest cost/token
```

Then use in requests:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer mr-yourkey" \
  -d '{"model":":cheapest","messages":[{"role":"user","content":"Hello"}]}'
```

If a shortcut is not configured, the request falls through to normal default routing. Shortcuts are resolved before model aliases, so they cannot be overridden by alias config.

### Disabling a model or a provider

When a model or a whole provider should not be used — an outage, a cost spike, a security
issue — an operator can take it out of rotation at runtime and put it back later. The change
persists, applies to the next request without a restart, and is recorded in the audit log
with who did it and why.

```bash
# CLI
modelrouter model list
modelrouter model disable --id 3 --reason "cost spike"
modelrouter model enable --id 3
modelrouter provider list
modelrouter provider disable anthropic --reason "vendor incident"
modelrouter provider enable anthropic
```

```bash
# Admin API (superadmin for writes, any admin role to read)
curl -X PATCH http://localhost:8080/admin/api/models/3/enabled \
  -H "Authorization: Bearer $ADMIN_JWT" -H 'Content-Type: application/json' \
  -d '{"enabled":false,"reason":"cost spike"}'

curl -X PATCH http://localhost:8080/admin/api/providers/anthropic/enabled \
  -H "Authorization: Bearer $ADMIN_JWT" -H 'Content-Type: application/json' \
  -d '{"enabled":false,"reason":"vendor incident"}'
```

The dashboard equivalent is the **Providers** and **Models** sections of `/admin/models`,
which show the reason, the actor and the timestamp next to anything disabled.

A disabled entity is excluded everywhere it could be reached:

- direct requests return **403** with `"type": "model_disabled"` and a message naming the
  reason — not a provider error, because nothing is called upstream;
- disabling a provider disables every model behind it;
- load-balancer pools skip disabled members (and report a 403 if every member is disabled);
- fallback chains skip disabled candidates and continue down the chain;
- aliases resolving onto a disabled target produce the same 403;
- `/v1/models` omits it.

**Disable vs. the circuit breaker.** These are separate mechanisms. The circuit breaker
reacts automatically to observed provider failures, is in-memory, and **auto-recovers**
after a cooldown. An operator disable is a deliberate decision: it is **sticky** — it never
auto-recovers, it survives a restart, and only an explicit enable clears it. The disable
check runs before the breaker and before any provider dispatch, so a disabled entity is
never called and can neither trip nor reset a breaker.

### OIDC SSO for admin login

By default, admins log in with username and password at `/admin/login`. To use an identity provider instead:

**Register an OAuth2 application in your IdP.** Set the redirect URI to:

```
http://localhost:8080/admin/auth/oidc/callback
```

**Add an `[oidc]` block to `config.toml`:**

```toml
[oidc]
enabled = true
issuer_url    = "https://accounts.google.com"   # or your Okta/Auth0 tenant URL
client_id     = "your-client-id"
client_secret = "your-client-secret"
redirect_uri  = "http://localhost:8080/admin/auth/oidc/callback"

# Restrict login to specific emails or entire domains
allowed_emails  = []
allowed_domains = ["yourcompany.com"]

# Role assigned to newly provisioned admins
auto_provision_role = "admin"
```

Restart modelrouter. Navigate to `/admin/auth/oidc/login` to authenticate via your IdP. Password-based login remains available alongside OIDC.

### Chinese Model Providers

All major Chinese LLM providers expose an OpenAI-compatible API and work with modelrouter's generic provider type. Configure them as named providers in your `config.toml`:

```toml
[providers.deepseek]
api_key  = "sk-..."
api_base = "https://api.deepseek.com/v1"

[providers.qwen]
api_key  = "sk-..."
api_base = "https://dashscope.aliyuncs.com/compatible-mode/v1"

[providers.doubao]
api_key  = "..."
api_base = "https://ark.cn-beijing.volces.com/api/v3"
```

Route to them using the `provider/model` syntax:

```toml
[routing.model_aliases]
deepseek = "deepseek/deepseek-chat"
qwen     = "qwen/qwen-max"
doubao   = "doubao/doubao-pro-32k"
```

Or reference them directly in requests:

```
deepseek/deepseek-chat
qwen/qwen-max
doubao/doubao-pro-32k
```

**Built-in pricing** is included for:
- **DeepSeek:** deepseek-chat, deepseek-coder, deepseek-reasoner
- **Alibaba Qwen:** qwen-max, qwen-plus, qwen-turbo
- **ByteDance Doubao:** doubao-lite-4k, doubao-lite-32k, doubao-pro-4k, doubao-pro-32k

Override any rate with a `[[pricing]]` entry in `config.toml`:

```toml
[[pricing]]
model = "deepseek-chat"
input_per_million = 0.14
output_per_million = 0.28
```

**Reading the effective pricing table.** `GET /admin/api/pricing` returns every
rate the router costs requests with: the built-in table with `[[pricing]]`
entries applied, in USD per million tokens, sorted by model key. It needs an
admin JWT (any role), the same as the other `/admin/api/*` reads.

```json
{
  "defaults": { "cache_read_multiplier": 0.1, "cache_write_multiplier": 1.25 },
  "models": [
    {
      "model": "claude-opus-5-5",
      "input_per_million": 4.0,
      "output_per_million": 20.0,
      "cache_read_per_million": 0.2,
      "cache_write_per_million": 5.0,
      "cache_read_defaulted": false,
      "cache_write_defaulted": false
    },
    {
      "model": "gpt-4o",
      "input_per_million": 2.5,
      "output_per_million": 10.0,
      "cache_read_per_million": 0.25,
      "cache_write_per_million": 3.125,
      "cache_read_defaulted": true,
      "cache_write_defaulted": true
    }
  ]
}
```

The cache rates are always the rates charged. `cache_read_defaulted` /
`cache_write_defaulted` are `true` when the entry sets no such rate and the
matching `defaults` multiplier of the input rate applies instead.

**How a model name is matched to a pricing row.** A client pricing tokens
itself should match names the same way:

1. Lowercase the name. Config `[[pricing]]` keys are lowercased too.
2. Drop everything up to and including the first `/` (the provider prefix):
   `anthropic/claude-haiku-4-5` looks up `claude-haiku-4-5`, and
   `vertex/anthropic/claude-x` looks up `anthropic/claude-x`.
3. If that key has no row and still contains a `/`, look up the part after the
   last `/` (`claude-x`). A vendor-qualified row therefore wins over a bare one.
4. A `@version` suffix is not stripped. `claude-haiku-4-5@20251001` matches only
   a row with that exact key; with no such row it is unpriced, and does not fall
   back to `claude-haiku-4-5`.
5. A name with no row costs $0, and the router logs a warning once per model.

---

## Usage

### CLI

**User management**

```bash
modelrouter user create --name alice
modelrouter user list
modelrouter user enable alice
modelrouter user disable alice
modelrouter user rotate-key alice
```

**API key management**

```bash
modelrouter key create --user alice --project myapp --label "myapp dev — alice"
modelrouter key list --user alice
modelrouter key disable --user alice --project myapp
```

**Group management**

```bash
modelrouter group create --name team-alpha [--priority 0]
modelrouter group list
modelrouter group add-member --group team-alpha --user alice
modelrouter group remove-member --group team-alpha --user alice
modelrouter group members --group team-alpha
modelrouter group enable team-alpha
modelrouter group disable team-alpha
```

**Budget management**

```bash
# Set limits — exactly one scope flag required
modelrouter budget set --global --window monthly --limit-usd 500
modelrouter budget set --global --window total --window-start 2026-04-01 --window-end 2026-06-30 --limit-usd 5000
modelrouter budget set --project myapp --window monthly --limit-usd 200
modelrouter budget set --user alice --window monthly --limit-usd 50 --rate-rpm 10
modelrouter budget set --group team-alpha --limit-usd 300          # soft target, not enforced

# Optional limit fields (any combination)
#   --limit-tokens <n>
#   --rate-rpm <n>
#   --max-concurrent <n>
#   --model-allow <model1,model2,...>
#   --model-deny <model1,model2,...>

modelrouter budget list [--user alice]
modelrouter budget edit --id 3 --limit-usd 75 [--window-start YYYY-MM-DD] [--window-end YYYY-MM-DD]
modelrouter budget delete --id 3
```

**Cost reporting**

```bash
# Basic — all users, this month
modelrouter report cost

# Filter by one or more dimensions (all are optional and composable)
modelrouter report cost --user alice
modelrouter report cost --group team-alpha
modelrouter report cost --project myapp
modelrouter report cost --model claude-sonnet-4-6
modelrouter report cost --key-id 7

# Window (default: monthly)
modelrouter report cost --window daily
modelrouter report cost --window weekly
modelrouter report cost --window monthly
modelrouter report cost --window alltime

# Output format (default: table)
modelrouter report cost --window monthly --format csv > report.csv
modelrouter report cost --window monthly --format json

# Combine freely
modelrouter report cost --group team-alpha --project myapp --window alltime --format csv
```

**Output columns** (one row per unique user + model + project + key combination):

| Column | Description |
|---|---|
| User | User name |
| Model | Model name as recorded in the cost ledger |
| Window | Selected time window |
| Group | User's active group memberships (comma-separated) |
| Project | Project label on the API key used for the request |
| Key | API key (shown as `project (label)`) |
| Cost (USD) | Total spend |
| Requests | Number of requests |
| Tokens Out | Output tokens |
| Tokens In | Input tokens |

**Filter reference** — all filters are optional and AND-composed:

| Flag | Description |
|---|---|
| `--user NAME` | Restrict to a single user |
| `--group NAME` | Restrict to active members of a group |
| `--project NAME` | Restrict to a specific project label |
| `--model NAME` | Restrict to a specific model |
| `--key-id ID` | Restrict to a specific API key (use `modelrouter key list` to find IDs) |
| `--window W` | `daily` · `weekly` · `monthly` · `alltime` |
| `--format F` | `table` (default) · `csv` · `json` |

`--user` and `--group` can be combined — the result is the intersection (user must be an active member of the group).

### API

All `/v1/*` endpoints except `GET /v1/models` require `Authorization: Bearer <api-key>`.

```bash
# List available models
curl http://localhost:8080/v1/models \
  -H "Authorization: Bearer <api-key>"

# Chat completion — identical to OpenAI API
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-opus-4-6",
    "messages": [{"role": "user", "content": "Hello"}]
  }'

# MCP server registry
curl "http://localhost:8080/v1/mcp/servers/discover?q=code+review+tools" \
  -H "Authorization: Bearer <api-key>"

# Embeddings
#
# `encoding_format` is honoured: the official OpenAI SDKs send "base64" by default
# and decode the response as base64, so the router returns a base64 string when
# asked for one. (Returning a float array to such a client does not error — it
# decodes to a shorter vector of zeros.)
#
# `dimensions` is forwarded to providers that support it AND verified against what
# came back, so a fallback to a different-width model fails loudly instead of
# returning vectors that do not match the ones already in your store.
curl http://localhost:8080/v1/embeddings \
  -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model": "embed", "input": "hello world", "dimensions": 768}'

# Web search — proxies to a configured search engine (Tavily by default)
curl http://localhost:8080/v1/search \
  -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"query": "rust programming language", "max_results": 5}'

# Decisions — typed questions over a document, answered by a decision-capable
# deployment (today: Azure AI Foundry). The body is passed through with `model`
# resolved to the deployment; the answer comes back as the provider sent it,
# plus `usage.cost_usd` and `x_router`. A provider 4xx is returned with its
# status and body under `error.upstream`, never retried.
curl http://localhost:8080/v1/decisions \
  -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model": "foundry/<decision-deployment>",
       "input": "I was charged twice for my order. Please return the extra charge.",
       "questions": [{"type": "predicate", "name": "refund_requested",
                      "instructions": "Does the customer ask for a refund?"}]}'

# Health check
curl http://localhost:8080/health
```

#### Decisions (`POST /v1/decisions`)

JSON passthrough to a provider's Decisions API: a document (`input`) plus typed
`questions` (a `predicate` answered as a `probability`; `choice` and `score`
types likewise), answered in question order under `answers[]`. Served today by
the `foundry` provider (`{endpoint}/openai/v1/decisions`, the same Entra or
`api-key` credential as chat). `model` resolves as an alias, a scoped alias
for the request's attribution tags, or `provider/deployment`, as on chat; a
bare name none of those know resolves against the providers'
`decision_deployments` (a decision-only deployment is usually absent from the
chat catalog), and a bare name no provider lists is a 400 naming it, before
anything is sent. A decision is never substituted: `routing.default_model` and
`routing.strict_model_resolution` play no part on this route, because the
caller named the one deployment that is to answer. The resolved deployment
replaces `model` on the wire; nothing else in the body is read or re-shaped. The call is gated by the policy engine, bounded by the tier
timeout the request addresses (else the provider's `timeout_secs`), priced from
the response `usage` at the deployment's chat input/output rates, and written
to the prompt log (without content) and the cost ledger. A model that resolves
to a provider with no decisions surface is a 400 naming the provider; a
deployment outside `[providers.foundry] decision_deployments`, when that list
is set, is a 400 before anything is sent; a provider 4xx is returned with its
status, its body under `error.upstream` and code `upstream_rejected`, and is
never retried; a 5xx is a 502. No fallback chain: a decision names one
deployment.

**A second upstream path.** Some decision model families are not served on
`{base}/decisions` (that path answers 404 for them) but on a route the resource
serves beside its data planes, `POST {resource}/providers/microsoft/v1/systemone`,
which takes the System One shape: the document as `state`, the questions as an
object keyed by question id, the answers keyed the same way, `usage` as
`input_tokens`/`output_tokens`. List those deployments under
`[providers.foundry] systemone_deployments` and the router adapts the shape
both ways, so a caller keeps the one Decisions shape whichever route a
deployment answers on: on the way out `input` becomes `state` (a `state` the
caller sent wins) and the `questions` array becomes an object keyed by each
question's `name` (`q<n>` for an unnamed one), each question's other fields as
written; on the way back the keyed answers become `answers[]` in question
order, each with its `name` put back. Only the shape is adapted: question
types, instructions, criteria and every other field travel untouched, and the
route's refusal of one it does not take is the 4xx it was.

Decisions are first-class in experiments and reporting. A decision request
binds to an experiment variant exactly as chat does (`x-modelrouter-experiment`,
the variant's overlay pins the requested decision model; the creation gate
admits a decision key only against a decision-capable, priced deployment, and
a bare `decision_deployments` entry is a valid target). The prompt-log row
keeps the request's `input` and `questions` and the provider's `answers` under
the storage policy (a retaining experiment forces content on), so the Compare
page and `GET /admin/api/compare` report, per question name, each arm's mean
probability, mean score or top choice and their agreement; without retained
content the comparison says so. Every prompt-log and ledger row carries a
`call_kind` (`chat`, `embedding`, `search`, `image`, `audio`, `decision`,
`rerank`, `judgment`, `health`; rows older than the column read `unknown`), so the cost
page, the attribution report (`--by kind`), the compare arms (`by_kind`,
`decision_requests`) and the failures page (per endpoint) show decisions as
their own line, per model and per project. The catalog lists a provider's
`decision_deployments` under `kind: decision`, apart from chat, so a chat
picker never offers them. A completed decision logs `upstream request
finished` and a `decision request finished` line with model, latency,
tokens, cost and the question and answer counts.

#### Rerank (`POST /v1/rerank`)

JSON passthrough to a provider's rerank API: a `query` and a non-empty
`documents` array (strings, or objects with a `text`, as the provider takes
them) with an optional `top_n`, answered as `results[]`, each an `index` into
the documents and a `relevance_score`, best first. Served today by the
`foundry` provider, whose rerankers speak the Cohere rerank API: the request
goes to the resource root's `/providers/cohere/v2/rerank` (or
`[providers.foundry] rerank_url`), with the same Entra or `api-key` credential
as chat. `model` resolves exactly as a decision does: an alias, a scoped alias,
or `provider/deployment`; a bare name none of those know resolves against the
providers' `rerank_deployments`; a bare name no provider lists is a 400 naming
it before anything is sent, and `routing.default_model` and
`routing.strict_model_resolution` play no part. The resolved deployment
replaces `model` on the wire; nothing else in the body is read or re-shaped.
A request without a `query` or with no `documents` is a 400 before any call.

A reranker is billed per **search unit** (one query against a batch of
documents; a provider counts a long batch as several), not per token, so the
call is priced from the provider's billed units (`meta.billed_units.search_units`
on Cohere, else `usage.search_units`) at a flat dollars-per-unit rate under the
pseudo-model `rerank/{deployment}` in `[[pricing]]` (or the live price feed),
the way `search/{engine}` is priced. An unpriced reranker is recorded at $0,
warned about once and reported `priced: false`, never as free. The call is
gated by the policy engine, bounded by the tier timeout the request addresses
(else the provider's `timeout_secs`), and written to the prompt log (the
query, the documents and the results, under the storage policy) and the cost
ledger with `call_kind = rerank`. A deployment outside
`[providers.foundry] rerank_deployments`, when that list is set, is a 400
before anything is sent; a provider 4xx is returned with its status, its body
under `error.upstream` and code `upstream_rejected`, and is never retried; a
5xx is a 502. No fallback chain. `x_router` reports the document count,
`top_n`, the billed search units and the result count. The request binds to
an experiment variant as decisions do (the overlay pins the requested
reranker). The catalog lists a provider's `rerank_deployments` under
`kind: rerank`, apart from chat.

#### Typed judgments (`POST /v1/systemone`)

JSON passthrough to [TypeSafe System One](https://docs.typesafe.ai/api)
(Choice / Score / Noul questions over a `state` payload). Callers send the
TypeSafe request body unchanged with their router API key; the router adds the
TypeSafe key from `[providers.typesafe]`. The call is gated and priced as the
pseudo-model `systemone/{model}` (e.g. `systemone/jev-latest`), per million
tokens off the response `usage` block. Upstream 429/529 (and 422) pass through
unchanged so clients can back off; an upstream 401/403 means the router's key
is wrong and is returned as 502 rather than blaming the caller.

#### Web search (`POST /v1/search`)

Proxies web search calls the same way `/v1/chat/completions` proxies LLM calls:
API-key auth, per-request logging, usage metering, pricing, and inclusion in
cost reports and OTel/Prometheus metrics. Three engines ship today — `tavily`,
`vertex` (Gemini + Google Search grounding) and `bing_grounding` (Grounding with
Bing Search on Azure AI Foundry) — and `src/providers/search_registry.rs`
documents how to add another. The two grounding engines run a model generation
as part of the call, so they are slower and need a completion-length
`timeout_secs`; `bing_grounding` results additionally carry Microsoft's citation
display obligations (see `docs/local-setup.md`).

Request body:

```json
{
  "query": "rust programming language",
  "max_results": 5,
  "engine": "tavily"
}
```

`query` is required and non-empty. `max_results` is optional (1-20, defaults
to the engine's own default). `engine` is optional; when omitted it falls back
to `[routing] default_search_engine`, then to the sole configured engine if
there is exactly one.

A search is priced as the engine's flat per-query rate (`[[pricing]]` or the
live price feed) plus, for a grounding engine, the tokens of the model call
that ran the search tool, priced from the model price table under the
deployment name the provider reports. The platform bills both. `x_router.tokens`
carries those tokens (`prompt`, `completion`, `cache_read`, `reasoning`) and the
ledger row records them in place of the result count; a plain search API
reports no tokens. Tokens of a deployment the price table does not know are
reported, warned about once, and not priced; the per-query rate still applies.

A grounding engine that ran its search tool and whose model cited nothing is a
**metered miss**, not an error: the provider served and billed the call. The
response is `200` with `results: []`, the usual `usage`/`x_router`, and a
`grounding` object naming it:

```json
{ "engine": "bing_grounding", "results": [], "grounding": { "status": "no_citations", "detail": "response status: completed; no url_citation annotations in the response" }, "usage": { "...": "" } }
```

No generated text travels with a miss, so ungrounded prose never reaches a
caller as a result. A miss on the primary engine still walks the fallback chain
first, and a miss is never written to the response cache: the next identical
query reaches the engine again.

`include_answer` is optional (boolean, default `false`). When `true`, a
grounding engine also returns the text its model wrote around the citations,
as `answer: {"text": "...", "query_urls": ["..."], "follow_up_queries": ["..."]}`.
`query_urls` are the search-engine query URLs the provider reported, verbatim
(Bing's display terms cover them too). The answer is generated, not retrieved:
use it to decide which cited pages to read or what to search next. It is never
returned without citations, because a response with no citations is still an
error. Today only `bing_grounding` produces one; other engines omit `answer`.

Grounded-research mode adds three optional fields, each requiring
`include_answer: true` (sent without it they are a 400):

- `instructions` (string): replaces the engine's default answer instruction,
  so a caller can version its research brief alongside its own code. The
  router always appends its grounding rule (answer only from retrieved pages,
  cite every claim), so instructions cannot turn the call into an ungrounded
  answer.
- `context` (string): appended to the model input after the question.
- `max_follow_up_queries` (0-10): asks the model for that many follow-up search
  queries, returned as `answer.follow_up_queries`. The router owns the line
  format it parses them from and strips that line from `answer.text`. A missing
  or malformed line yields `[]`, never an error.

`include_answer` and all three fields are part of the response-cache key, so a
cached answer is never served to a request that asked for something different.

Response:

```json
{
  "engine": "tavily",
  "results": [
    {
      "title": "Rust (programming language) - Wikipedia",
      "url": "https://en.wikipedia.org/wiki/Rust_(programming_language)",
      "snippet": "Rust is a multi-paradigm, general-purpose programming language...",
      "score": 0.95,
      "published_date": null
    }
  ],
  "model": "search/tavily",
  "usage": { "results": 1, "cost_usd": 0.005 },
  "x_router": { "...": "see below" }
}
```

Out of scope for this endpoint: re-ranking and fetching full page content
(only the engine's own snippet is returned).

#### Per-call cost and routing metadata in responses

Every priced response carries the router's own account of the call, so a
client can record the authoritative cost, the model that actually answered,
the settings sent, the token counts and the timing **verbatim** instead of
re-pricing calls against a private price list that drifts from the router's
`[[pricing]]` table. Every figure is the value the router wrote to its
`cost_ledger` (and `prompts`) row for the same request.

Covered endpoints: `/v1/chat/completions` (non-streamed, and the terminal
chunk of a stream), `/v1/embeddings`, `/v1/search`, `/v1/decisions`. Not yet covered:
`/v1/messages`, `/v1/responses`, images, audio.

Two additive fields; the OpenAI shape is otherwise unchanged:

- `usage.cost_usd` — USD charged for this call (the ledger row's
  `cost_usd`), after retries and fallback. On a response-cache hit it is
  `0`, with `usage.cache_hit: true` and `usage.saved_usd` carrying the
  avoided cost. `usage.uncached_cost_usd` is always present: the call's
  cost had it not been served from the cache, equal to `cost_usd` on a live
  call. `usage.no_cache_cost_usd` is always present too: the call's cost
  with no caching at all, neither the response cache nor the provider's
  prompt cache (every prompt token at the standard input rate). It differs
  from `uncached_cost_usd` only when the call read from or wrote to the
  prompt cache; a call that only wrote to it costs more than this figure,
  since cache writes are priced above the input rate. `usage.tokens_estimated: true` appears when the provider
  reported no usage and the counts (and so the cost) are the router's
  estimate.
- `x_router` — the full per-call record:

```json
{
  "requested_model": "balanced",
  "model": "claude-sonnet-4-5",
  "provider": "vertex",
  "settings": {
    "temperature": 0.2, "max_tokens": 8192, "timeout_secs": 600,
    "stream": false, "tools": null, "tool_choice": null, "response_format": null, "dropped": []
  },
  "tokens": {
    "prompt": 1200, "completion": 350, "total": 1550,
    "cache_read": 0, "cache_write": 0, "reasoning": null, "estimated": false
  },
  "cost": { "cost_usd": 0.00885, "cache_hit": false },
  "timing": {
    "total_ms": 4210, "latency_ms": 4180, "provider_ms": 4170,
    "ttft_ms": 900, "attempts": 1, "fallbacks": 0
  }
}
```

| Field | Meaning |
|---|---|
| `requested_model` | The model name or alias the caller sent. |
| `model`, `provider` | What answered after alias resolution and fallback (the ledger row's `model`/`provider`). The top-level `model` field names the same model. |
| `settings` | Values actually sent to the provider after router resolution and adapter defaults. `dropped` lists caller parameters the router deliberately did not forward (e.g. `temperature` to a model that rejects it). Search reports `max_results`. |
| `tokens` | Chat/embeddings only. `prompt` includes provider-cache reads; `reasoning` is `null` when the provider gave no figure; `estimated` mirrors the ledger's `tokens_estimated`. |
| `results` | Search only: results returned. |
| `cost` | `cost_usd`, `uncached_cost_usd`, `no_cache_cost_usd`, `cache_hit`, and `saved_usd` on a hit. |
| `timing` | `total_ms` from request receipt; `latency_ms` dispatch to completion including retries and fallback hops (0 on a hit); `provider_ms` for the answering provider call; `ttft_ms` for streams (first byte); `attempts` provider calls made; `fallbacks` hops to another model. |

**Streams.** The router splices one extra `chat.completion.chunk` with
`"choices": []`, a `usage` object (including `cost_usd`) and `x_router`
immediately before `data: [DONE]`. Its figures come from the same settlement
that writes the ledger row. Clients that ignore chunks without choices are
unaffected. A stream that is abandoned before completion never receives it.

A client that finds neither `usage.cost_usd` nor `x_router` on a response is
talking to a router build that predates this contract, and should treat the
call as unpriced rather than estimate it.

#### Request cost attribution

Every metered endpoint (`/v1/chat/completions`, `/v1/search`, `/v1/messages`,
`/v1/responses`, `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/*`)
accepts an optional `attribution` block describing the **caller's** unit of
work — an engagement, a job, a run. The router already knows who paid (user,
API key, key project); attribution is what lets an app ask "what did *this*
piece of my own work cost", instead of maintaining a parallel cost ledger.

```json
{
  "model": "gpt-4o-mini",
  "messages": [{ "role": "user", "content": "hi" }],
  "attribution": {
    "correlation_id": "eng-4711-run-3",
    "tags": { "engagement": "eng-4711", "phase": "research" }
  }
}
```

The body field is the primary channel (SDKs expose it through `extra_body`).
Headers are the fallback, and the only channel for the multipart
`/v1/audio/transcriptions`:

```text
X-Attribution-Correlation-Id: eng-4711-run-3
X-Attribution-Tags: {"engagement":"eng-4711","phase":"research"}
```

Bounds, enforced with a `400` on violation: correlation id ≤ 128 characters;
at most 8 tags; keys ≤ 64 characters and restricted to `[A-Za-z0-9_-.:]`;
values ≤ 128 characters; the encoded tag map ≤ 1 KB. Number and boolean tag
values are coerced to strings; nested values are rejected.

**Attribution never changes routing, pricing, or the response-cache key.** Two
requests that differ only in attribution share one cache entry — the saving is
metered against whoever made the call that hit.

Read it back three ways:

```bash
# CLI — spend and cache savings for one unit of work
modelrouter report cost --tag engagement=eng-4711 --window monthly
modelrouter report cost --correlation-id eng-4711-run-3 --by day
```

```text
GET /admin/api/usage/attribution?key=engagement&value=eng-4711&window=monthly
GET /admin/api/usage/attribution/facets            # tag keys + correlation ids
GET /admin/api/usage/attribution/facets?key=engagement   # values for one key
```

and on `/admin/reports`, where picking an attribution value swaps the panels
for an attributed-usage view (spend, savings, cache-hit rate, per-model and
per-day breakdowns).

To compare two arms of an experiment — two tag values, two correlation ids,
two models, two providers or two variants of a controlled experiment — use
`GET /admin/api/compare`, `modelrouter report compare`, or the
`/admin/compare` page. [docs/experiments.md](docs/experiments.md) walks a
client application through labelling traffic, retrieving the comparison,
and reading it.

### Streaming

Set `"stream": true` on `/v1/chat/completions` — same contract as OpenAI —
and the router streams SSE chunks in OpenAI shape from every provider it
fronts, including the ones whose native streams look nothing like OpenAI's:
Claude and Gemini on Vertex (`:streamRawPredict` / `:streamGenerateContent`
events are translated in flight), Bedrock (ConverseStream events), Azure AI
Foundry, and every OpenAI-compatible upstream. SSE events split across
network chunk boundaries are reassembled before translation, and a streaming
request whose upstream answers with something that produces no SSE events at
all fails loudly with an SSE `error` event rather than a silent empty 200.

**The router owns usage capture on streamed responses.** The upstream request
always asks the provider for usage — `stream_options: {"include_usage": true}`
is injected into every OpenAI-shaped upstream body, and the native usage
frames are harvested on the translated paths (Anthropic's split
`message_start`/`message_delta` report, Gemini's `usageMetadata`, Bedrock's
stream metadata) — regardless of what the client sent. The counts flow two
ways:

- **To the ledgers**: streamed requests are metered with provider-counted
  tokens (`tokens_estimated = false`), so budgets, cost reports, attribution
  and per-experiment-variant accounting are exact for streamed traffic. A
  character-count estimate (flagged as such) remains only as a fallback for
  upstreams that report nothing.
- **To the caller**: the final chunk carries a normalized OpenAI-shaped
  `usage` object (`prompt_tokens`, `completion_tokens`, `total_tokens`,
  `prompt_tokens_details.cached_tokens`) whether or not the client asked for
  `include_usage`. On translated providers it rides the finish chunk, so
  `choices` is never empty and strict chunk parsers are safe.

This means experiment-bound traffic (`x-modelrouter-experiment`) can stream:
per-variant usage is recorded server-side, so there is no accounting reason
to force `stream: false`.

```bash
curl -N http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "vertex/anthropic/claude-sonnet-4-5@20250929",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
  }'
# data: {"choices":[{"delta":{"content":"Hi"},"finish_reason":null,...}],...}
# data: {"choices":[{"delta":{},"finish_reason":"stop",...}],
#        "usage":{"prompt_tokens":9,"completion_tokens":3,"total_tokens":12,...}}
# data: [DONE]
```

### Tool calling (function tools)

`POST /v1/chat/completions` forwards OpenAI-shaped `tools` and `tool_choice`
to backing models that support tool calling, translating where the upstream
speaks a different dialect:

- **OpenAI-compatible upstreams** (OpenAI, Azure OpenAI, Groq/Ollama-style
  bases, Vertex MaaS): tools, tool-call responses and streamed tool-call
  deltas pass through verbatim.
- **Anthropic-shaped upstreams** (direct Anthropic, Claude on Vertex): tools
  translate to native `tools`/`input_schema`, assistant `tool_calls` turns
  and `tool`-role results translate to `tool_use`/`tool_result` blocks, and
  responses translate back — including streaming, where `tool_use` deltas
  are re-emitted as OpenAI `tool_calls` chunks and `stop_reason: "tool_use"`
  becomes `finish_reason: "tool_calls"`.

The agentic loop (assistant calls a tool → client sends the `tool` result →
model continues) round-trips through the router on every provider above.
Fallback routing never substitutes a backend that cannot take tools: a
request carrying tools skips such candidates in the fallback chain.

Backends without tool forwarding (Bedrock, Gemini, Azure AI Foundry agents)
reject tool-carrying requests with a clear 400 naming the resolved
provider/model, rather than silently dropping the tools. A `tool_choice`
without a non-empty `tools` array is rejected as malformed.

### Structured output (`response_format`)

`POST /v1/chat/completions` forwards the OpenAI-shaped `response_format`
(`{"type": "json_object"}`, or `{"type": "json_schema", "json_schema":
{"name", "schema", "strict"}}`) to backing models that take it:

- **OpenAI-compatible upstreams** (OpenAI, Azure OpenAI, Azure AI Foundry's
  OpenAI surface, Vertex MaaS): the object passes through verbatim.
- **Gemini**: both modes become `generationConfig.responseMimeType:
  "application/json"`. The schema itself is not forwarded, because Gemini's
  `responseSchema` takes an OpenAPI subset that rejects common JSON Schema
  keywords; the caller's own validation remains the contract.
- **Anthropic-shaped upstreams** (direct Anthropic, Claude on Vertex or
  Foundry) and **Bedrock** have no structured-output mode: the request is
  dropped and named in `x_router.settings.dropped`, so a caller that checks
  the answer's shape itself knows the provider was never asked for it.

Unlike tools, a dropped `response_format` is reported, not refused: the
answer is still usable, only its shape is unguaranteed. A malformed
`response_format` (an unknown `type`, or `json_schema` without a
`json_schema.schema` object) is a 400 at the door. `x_router.settings.
response_format` names the mode the adapter was asked for.

### Controlled experiments

A router-managed A/B run. Create an experiment with named variants — each
one an overlay from the model name a caller requests to the `provider/model`
it is sent to instead — and put `x-modelrouter-experiment: <id>` (router
assigns the variant by a stable hash of `session_id`) or `<id>:<label>` on
each chat completion. Bound requests are pinned to their variant's model
with no downgrade, pool, affinity, cache or fallback, and every row the
router writes is stamped with the experiment and variant. The application
reports how each run went with `POST /v1/feedback`, and reads back
per-variant and per-run cost, tokens, turns, span, latency, failures and
outcomes from `GET /admin/api/experiments/:id/results`, `modelrouter
experiment results`, or the `/admin/experiments` page. Embeddings and
decisions (`POST /v1/decisions`) bind the same way; a decision variant must
pin a decision-capable deployment (see the Decisions section).

```bash
modelrouter experiment add --name checkout-haiku \
  --variant 'control=fast:fast' --variant 'candidate=fast:anthropic/claude-haiku-4-5' \
  --expires-at 2026-10-01T00:00:00Z --content-retention-days 30
```

Expiry and retention are required, never defaulted; every target must
resolve to a priced, configured `provider/model` (no pools, no fall-through
to the default model) or creation is refused; an unknown experiment or
variant on a request is a `400`, never silent routing. An experiment can
retain full prompt content for its own traffic under a superadmin-set
window (`--retain-content`), purged after close. Part 1 of
[docs/experiments.md](docs/experiments.md) is the end-to-end guide.

Admin REST endpoints at `/admin/api/*` require a JWT from `POST /admin/api/login`. The browser-based dashboard is at `/admin`.

### OTel observability stack

modelrouter emits OpenTelemetry traces, metrics, and logs when built with `--features otel`. The recommended local setup uses the bundled `docker-compose.otel.yml`, which wires together modelrouter, an OpenTelemetry Collector, [Arize Phoenix](https://docs.arize.com/phoenix) (traces + LLM spans), and Prometheus (metrics) on a shared Docker network.

**Start the full observability stack:**

```bash
docker-compose -f docker-compose.otel.yml up -d
```

| Service | URL | Purpose |
|---|---|---|
| modelrouter | http://localhost:8080 | LLM proxy |
| Arize Phoenix | http://localhost:6006 | Trace viewer, LLM span analysis |
| Prometheus | http://localhost:9090 | Metrics graphs |

> **Note:** Use `docker-compose -f docker-compose.otel.yml` — not plain `docker-compose up`. The plain `docker-compose.yml` starts modelrouter without the collector, so `otel-collector` will not resolve and telemetry will fail silently.

The `otel-collector` service in `docker-compose.otel.yml` receives OTLP on port 4317 and fans out:
- Traces → Phoenix (at `phoenix:4317`)
- Metrics → Prometheus scrape endpoint (port 8889)
- Logs → stdout of the collector container

**Bring the stack down:**

```bash
docker-compose -f docker-compose.otel.yml down
```

**Using a different OTLP backend** (Grafana, Honeycomb, Datadog, etc.): edit `otel-collector/config.yml` to add or replace exporters, or point `telemetry.endpoint` in `config.toml` directly at your collector's gRPC endpoint and omit the otel-collector service entirely.

Each trace includes `user.id`, `model.canonical`, `provider.name`, `tokens.prompt`, `tokens.completion`, and `cost.usd` attributes.

---

## Architecture

```
src/
├── api/
│   ├── admin/
│   │   ├── auth.rs             — JWT issuance and verification, AdminSession extractor
│   │   ├── budgets.rs          — Budget admin handlers (get/create/edit/delete per scope)
│   │   ├── dashboard.rs        — browser dashboard handlers (HTMX, mr_admin_session cookie)
│   │   ├── groups.rs           — Groups admin handlers (create, enable/disable, membership)
│   │   ├── oidc.rs             — OIDC state store, PKCE, discovery, token validation
│   │   └── routes.rs           — admin REST API handlers
│   ├── app.rs                  — axum router assembly, AppState, middleware stack
│   ├── auth.rs                 — Bearer token auth for /v1/* endpoints
│   └── routes/
│       ├── completions.rs      — POST /v1/chat/completions handler
│       ├── mcp.rs              — MCP server registry REST + discover endpoint
│       ├── models.rs           — GET /v1/models handler
│       └── ...                 — embeddings, images, audio, responses
├── cli/                        — Clap CLI commands (serve, init, migrate, user, budget, report)
├── config/                     — Config loading and schema (Settings, GuardrailConfig, OidcConfig, …)
├── db/                         — sqlx migrations, model types, repository traits
│   ├── repositories/           — trait definitions (one file per domain)
│   ├── sqlite/                 — SQLite implementations
│   └── postgres/               — Postgres implementations (--features postgres)
├── guardrails/
│   ├── mod.rs                  — GuardrailChain, Guardrail trait, GuardrailDecision
│   └── openai_moderation.rs    — OpenAI moderation API integration
├── hooks/
│   ├── lifecycle.rs            — before/after request lifecycle hooks
│   └── pipeline.rs             — streaming pipeline hooks
├── providers/                  — Upstream adapters (Anthropic, OpenAI, Bedrock, Azure, Gemini, Ollama)
├── router/
│   ├── declarative_policy.rs   — TOML-configured policy rule matching
│   ├── policy.rs               — PolicyEngine: user/key → project → global budget enforcement
│   ├── engine.rs               — RequestRouter: alias resolution, provider selection, load balancing
│   └── ...                     — cache, circuit_breaker, fallback, retry, session_limits
├── report/                     — Cost reporting and audit log formatting
└── telemetry/                  — OTel init, SmartSampler, metrics instruments (--features otel)
```

**Budget enforcement order:** per-user/key rules are checked first, then project-scope rules, then global rules. A request is blocked when any applicable rule is exceeded. Group-scope rules are informational targets only and never block requests.

The binary entry point at `src/main.rs` delegates entirely to the library crate, keeping all logic testable without spinning up a process.

---

## Development

```bash
# Build (default: SQLite only)
cargo build

# Build with Postgres support
cargo build --features postgres

# Build with OpenTelemetry support
cargo build --features otel

# Run all tests
cargo test

# Run OTel tests
cargo test --features otel

# End-to-end tier: runs the real binary against a mock provider.
# Ignored by default so `cargo test` stays fast. Name the targets rather than
# using a bare `-- --ignored`, which also picks up a Redis-dependent test.
cargo test --test test_e2e_startup --test test_e2e_requests --test test_e2e_accounting -- --ignored

# Start development server
cargo run -- serve
```

Logs go to stdout via `tracing`. Set `RUST_LOG=modelrouter=debug` for verbose output.

The in-process tests build `AppState` by hand and never execute `serve`, config
resolution or the startup guards; the e2e tier is the only place the binary is
tested as a program. Its design and what each test proves are in
[`docs/testing/e2e-harness.md`](docs/testing/e2e-harness.md).

The database schema lives in `migrations/`. After adding a new migration file, run `modelrouter migrate` to apply it. `sqlx::migrate!` embeds migrations into the binary at compile time.

---

## Contributing

Contributions are welcome. Fork the repository, create a branch, and open a pull request against `main`. Please ensure `cargo test` passes and `cargo build --features postgres,otel` compiles cleanly before submitting.

---

## License

[MIT](LICENSE)
