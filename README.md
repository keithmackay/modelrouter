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
- **Routing shortcuts** — use `:fastest` or `:cheapest` as the model name to route to your configured fastest or cheapest model without changing client code
- **Multi-scope budget enforcement** — set monthly or fixed date-range limits at the global (org-wide), project, user, or group level; any limit hit blocks the request before it reaches the upstream
- **Admin dashboard** — web UI at `/admin` with usage stats, audit log, and full management pages for users, API keys, groups, budgets, and webhooks
- **Webhook callbacks** — register outbound webhooks via admin UI or CLI (`modelrouter webhook add`) that fire JSON POSTs after each completion; wire Datadog, Slack, or any HTTP endpoint; takes effect on next restart
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
bucket = int(time.time()) // 28800  # 8-hour window — controls max session ID age, not TTL
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

---

## Getting Started

### Prerequisites

- At least one upstream provider API key (OpenAI, Anthropic, Gemini, or a local Ollama instance)
- Rust 1.75+ if building from source
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
docker run \
  -v /host/config:/config \
  -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -p 8080:8080 \
  ghcr.io/keithmackay/modelrouter:latest serve
```

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

### 2. Run migrations and start the server

```bash
modelrouter migrate
modelrouter serve
curl http://localhost:8080/health   # → {"status":"ok"}
```

### 3. Create the first superadmin

The first admin must be created via the CLI. All subsequent admin management is available in the web UI.

```bash
modelrouter admin create --name ops --password <strong-password> --role superadmin
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
| `server.host` | Bind address | `127.0.0.1` |
| `server.port` | Listen port | `8080` |
| `database.path` | SQLite file path | `~/.modelrouter/router.db` |
| `routing.default_provider` | Fallback provider when model prefix is absent | `openai` |
| `routing.model_aliases` | Map short names to canonical model IDs | — |
| `providers.<name>.api_key` | Upstream provider API key | required |
| `providers.<name>.base_url` | Override provider endpoint | provider default |
| `auth.jwt_secret` | Secret for admin JWT signing | required |
| `[[guardrails]]` | Content safety rules (type, fail_open, api_key, endpoint) | — |
| `[[policy_rules]]` | Declarative access rules matched by project/group/user/model | — |
| `[oidc]` | OIDC SSO for admin login (issuer_url, client_id, client_secret, …) | disabled |
| `telemetry.endpoint` | OTLP gRPC endpoint (`--features otel`) | disabled |
| `telemetry.sample_ratio` | Fraction of normal requests to trace | `0.1` |

See [`config.example.toml`](config.example.toml) for a fully annotated reference configuration.

### Model routing

Models resolve in this order:

1. Alias lookup from `routing.model_aliases`
2. Provider prefix — `anthropic/claude-opus-4-6` routes to the `anthropic` provider
3. Fall back to `routing.default_provider`

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

All `/v1/*` endpoints accept `Authorization: Bearer <api-key>`.

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

# Health check
curl http://localhost:8080/health
```

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

# Start development server
cargo run -- serve
```

Logs go to stdout via `tracing`. Set `RUST_LOG=modelrouter=debug` for verbose output.

The database schema lives in `migrations/`. After adding a new migration file, run `modelrouter migrate` to apply it. `sqlx::migrate!` embeds migrations into the binary at compile time.

---

## Contributing

Contributions are welcome. Fork the repository, create a branch, and open a pull request against `main`. Please ensure `cargo test` passes and `cargo build --features postgres,otel` compiles cleanly before submitting.

---

## License

[MIT](LICENSE)
