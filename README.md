# modelrouter

![Release](https://img.shields.io/github/actions/workflow/status/keithmackay/modelrouter/release.yml?label=release)
![Version](https://img.shields.io/badge/version-0.1.1-blue)
![License](https://img.shields.io/badge/license-MIT-green)
![Rust](https://img.shields.io/badge/rust-2021-orange)

An OpenAI-compatible LLM proxy that routes requests across providers, enforces per-user spend budgets, and runs configurable hooks — all from a single self-hosted binary.

Point your existing OpenAI SDK at modelrouter instead of `api.openai.com`. It authenticates your users with API keys, resolves model aliases, selects the right upstream provider, and tracks token spend — stopping requests that would blow a user's monthly budget before they hit the provider.

---

## Table of Contents

- [Highlights](#highlights)
- [Getting Started](#getting-started)
- [Usage](#usage)
- [Setup Walkthrough](#setup-walkthrough)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Development](#development)
- [Contributing](#contributing)
- [License](#license)

---

## Highlights

- **Drop-in OpenAI compatibility** — any SDK that speaks `POST /v1/chat/completions` works without modification
- **Multi-provider routing** — route to OpenAI, Anthropic, Google Gemini, or Ollama; switch providers by changing one config line
- **Per-user budget enforcement** — set monthly, weekly, or daily spend limits; over-budget requests are rejected before they reach the upstream
- **Declarative policy engine** — TOML-configured rules that match users by tag, group, or ID and enforce model allow-lists and budgets without touching the database
- **Content guardrails** — pluggable safety layer runs OpenAI moderation (or a custom HTTP endpoint) on requests and responses; configurable fail-open/fail-closed
- **MCP server registry** — register and discover Model Context Protocol servers via REST; semantic search ranks results by relevance to a query
- **SSO / OIDC** — admin users can authenticate via Google, Okta, Auth0, or any OIDC provider using authorization code flow with PKCE; new admins are auto-provisioned from email allow-lists
- **Hook system** — run shell scripts or HTTP webhooks at lifecycle events and in the request pipeline; grant capabilities per-user via `hook_permissions`
- **Admin dashboard** — web UI at `/admin` with usage stats, audit log, budget management, and user administration
- **Feature-flagged optional components** — `--features postgres` for Postgres backend, `--features otel` for full OpenTelemetry observability (traces, metrics, logs via OTLP)
- **Single static binary** — SQLite bundled, no runtime dependencies; ships as a distroless Docker image

---

## Getting Started

### Prerequisites

- Rust 1.75+ (for building from source)
- At least one upstream provider API key (OpenAI, Anthropic, Gemini, or a local Ollama instance)
- Optional: PostgreSQL 14+ if using `--features postgres`

### Installation

**Docker (from GHCR):**

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
# -p 8080:8080 maps to server.port in config.toml (default: 8080)
```

**Build from source:**

```bash
git clone https://github.com/keithmackay/modelrouter.git
cd modelrouter
cargo build --release
# Binary is at target/release/modelrouter
```

**With OTel support:**

```bash
cargo build --release --features otel
```

**Initial setup:**

```bash
# Generate a starter config at ~/.modelrouter/config.toml
modelrouter init

# Run database migrations
modelrouter migrate

# Start the proxy
modelrouter serve
```

---

## Usage

### CLI

```bash
# User and budget management
modelrouter user create --name alice
modelrouter user list
modelrouter budget set --user alice --limit-usd 10.0 --window monthly

# Cost reporting
modelrouter report cost --user alice --window monthly --format table
modelrouter report cost --window monthly --format csv > report.csv

# Install as a system service (macOS or Linux)
modelrouter install-service
```

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
    "model": "claude-opus-4-5",
    "messages": [{"role": "user", "content": "Hello"}]
  }'

# MCP server registry — list registered servers
curl http://localhost:8080/v1/mcp/servers \
  -H "Authorization: Bearer <api-key>"

# MCP server registry — discover servers by semantic query
curl "http://localhost:8080/v1/mcp/servers/discover?q=code+review+tools" \
  -H "Authorization: Bearer <api-key>"

# Health check
curl http://localhost:8080/health
```

Admin REST endpoints at `/admin/api/*` require a JWT from `POST /admin/api/login`. Admin login via OIDC SSO is available at `GET /admin/auth/oidc/login` when configured. A browser-based dashboard is available at `/admin`.

---

## Setup Walkthrough

This walkthrough covers a complete team deployment: two users with separate budgets, Claude Code configured as the client, and Arize Phoenix receiving OTel traces and metrics.

**Prerequisites for this walkthrough:**
- modelrouter built with `--features otel`
- Anthropic API key
- Arize Phoenix running locally or on your network

### 1. Configure the Anthropic provider

Edit `~/.modelrouter/config.toml`. Set your Anthropic API key and add a `claude-code` alias for convenience:

```toml
[providers.anthropic]
api_key = "sk-ant-..."
timeout_secs = 120

[routing]
default_provider = "anthropic"
default_model = "claude-opus-4-5"

[routing.model_aliases]
"claude-code" = "anthropic/claude-opus-4-5"
```

Run migrations and verify the server starts:

```bash
modelrouter migrate
modelrouter serve
curl http://localhost:8080/health   # → {"status":"ok"}
```

### 2. Create users

Create Abdoul and Becky, assigning them to a group that identifies their project. The group acts as a project label for reporting.

```bash
modelrouter user create --name abdoul --group team-alpha
# Created user 'abdoul' (id=1)
# API key: mr-a1b2c3d4e5f6...
# Store this key securely — it cannot be retrieved later.

modelrouter user create --name becky --group team-alpha
# Created user 'becky' (id=2)
# API key: mr-9z8y7x6w5v4u...
# Store this key securely — it cannot be retrieved later.
```

Save each API key — it is shown exactly once and the plaintext is never stored.

Verify both users appear:

```bash
modelrouter user list
#    1  abdoul               enabled  team-alpha
#    2  becky                enabled  team-alpha
```

### 3. Set budgets

Give Abdoul a $50/month limit and Becky a $100/month limit:

```bash
modelrouter budget set --user abdoul --window monthly --limit-usd 50.0
# Created budget rule (id=1) for user 'abdoul': monthly window, limit=$50.0

modelrouter budget set --user becky --window monthly --limit-usd 100.0
# Created budget rule (id=2) for user 'becky': monthly window, limit=$100.0
```

Confirm the rules:

```bash
modelrouter budget list
#    1  user=abdoul  window=monthly  limit_usd=Some(50.0)  rate_rpm=None
#    2  user=becky   window=monthly  limit_usd=Some(100.0)  rate_rpm=None
```

When a user hits their limit, subsequent requests receive a `429 Budget exceeded` response and are not forwarded to the provider.

### 4. Configure Claude Code

Claude Code uses the Anthropic SDK, which respects the `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY` environment variables. Set these per developer so their Claude Code sessions route through modelrouter.

**Abdoul's machine:**

```bash
export ANTHROPIC_BASE_URL="http://modelrouter.internal:8080"
export ANTHROPIC_API_KEY="mr-a1b2c3d4e5f6..."
```

**Becky's machine:**

```bash
export ANTHROPIC_BASE_URL="http://modelrouter.internal:8080"
export ANTHROPIC_API_KEY="mr-9z8y7x6w5v4u..."
```

Add these to each developer's shell profile (`~/.zshrc`, `~/.bashrc`, etc.) to make them permanent. After this, every Claude Code invocation authenticates as that user and records spend against their budget.

> **Note:** `ANTHROPIC_BASE_URL` overrides the SDK's default `api.anthropic.com` endpoint. modelrouter receives the request, authenticates the bearer token against its user database, checks the budget, proxies the call upstream to Anthropic, and records the cost.

### 5. Connect OTel to Arize Phoenix

[Arize Phoenix](https://docs.arize.com/phoenix) is an open-source LLM observability platform. Start it locally:

```bash
pip install arize-phoenix
phoenix serve
# Phoenix UI: http://localhost:6006
# OTLP gRPC:  http://localhost:4317
```

Or via Docker:

```bash
docker run -p 6006:6006 -p 4317:4317 arizephoenix/phoenix:latest
```

Add the `[telemetry]` block to `~/.modelrouter/config.toml`:

```toml
[telemetry]
enabled = true
endpoint = "http://localhost:4317"   # Phoenix OTLP gRPC endpoint
service_name = "modelrouter"
sample_ratio = 1.0                   # Trace every request during initial setup
slow_threshold_ms = 2000             # Always trace requests slower than 2s
```

Restart modelrouter. Send a test request:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer mr-a1b2c3d4e5f6..." \
  -H "Content-Type: application/json" \
  -d '{"model": "claude-code", "messages": [{"role": "user", "content": "ping"}]}'
```

Open Phoenix at `http://localhost:6006` — the trace should appear within a few seconds. You will see the `chat_completions` root span with child spans for `modelrouter.policy_check` and `modelrouter.provider_call`, and attributes including `user.id`, `model.canonical`, `provider.name`, `tokens.prompt`, `tokens.completion`, and `cost.usd`.

For a production deployment, lower `sample_ratio` to reduce volume:

```toml
sample_ratio = 0.1   # Trace 10% of normal requests; errors always traced
```

### 6. Review budget usage

**Per user — check how much Abdoul has spent this month:**

```bash
modelrouter report cost --user abdoul --window monthly
# User    Model                   Cost (USD)   Tokens In   Tokens Out   Requests
# abdoul  anthropic/claude-opus-4-5  0.031200     4800        2100         12
```

**Per user — same for Becky:**

```bash
modelrouter report cost --user becky --window monthly
```

**Entire org — all users, this month:**

```bash
modelrouter report cost --window monthly
# User    Model                     Cost (USD)   Tokens In   Tokens Out   Requests
# abdoul  anthropic/claude-opus-4-5    0.031200     4800        2100         12
# becky   anthropic/claude-opus-4-5    0.087600    12000        6800         31
```

**Per project — usage by model across a group:**

The `report usage` command filters by model and date range. To narrow to a project's spend, filter by `--since` and review the per-user breakdown alongside it:

```bash
# Model-level breakdown since the start of the month
modelrouter report usage --since 2026-04-01T00:00:00Z

# Detailed prompt log for Abdoul this week
modelrouter report prompts --user abdoul --since 2026-03-25T00:00:00Z
```

> **Note on project filtering:** The `report usage --project <name>` flag is available but requires a `project` label to be set on requests at the time they are made. This is not yet populated from request metadata. Until it is, use user groups and per-user reports as shown above to track project-level spend.

**Check remaining budget headroom:**

```bash
modelrouter budget list --user abdoul
#    1  user_id=Some(1)  window=monthly  limit_usd=Some(50.0)  rate_rpm=None
```

Cross-reference with the cost report: Abdoul has spent $0.03 of his $50.00 monthly limit.

### 7. (Optional) Enable OIDC SSO for admin login

By default, admin users log in with a username and password at `/admin/login`. If your team uses an identity provider (Google, Okta, Auth0, Keycloak, or any OIDC-compliant IdP), you can configure SSO so admins authenticate through their normal corporate credentials instead.

**Register a new OAuth2 application in your IdP.** Set the redirect URI to:

```
http://localhost:8080/admin/auth/oidc/callback
```

(Replace `localhost:8080` with your actual hostname in production.)

**Add an `[oidc]` block to `~/.modelrouter/config.toml`:**

```toml
[oidc]
enabled = true
issuer_url    = "https://accounts.google.com"   # or your Okta/Auth0 tenant URL
client_id     = "your-client-id"
client_secret = "your-client-secret"
redirect_uri  = "http://localhost:8080/admin/auth/oidc/callback"

# Restrict login to specific email addresses or entire domains
allowed_emails  = []
allowed_domains = ["yourcompany.com"]

# Role assigned to newly provisioned admins ("admin" or "superadmin")
auto_provision_role = "admin"
```

Restart modelrouter. Navigate to `http://localhost:8080/admin/auth/oidc/login` — you will be redirected to your IdP. After a successful login, modelrouter creates an admin account for you (if one doesn't already exist) and sets a session cookie.

> **Note:** Password-based login at `/admin/login` remains available alongside OIDC. Existing admin accounts are not affected. OIDC-provisioned accounts have an empty password hash and cannot log in via the password form.

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
| `[[policy_rules]]` | Declarative access rules matched by tag/group/user/model | — |
| `[oidc]` | OIDC SSO for admin login (issuer_url, client_id, client_secret, …) | disabled |
| `telemetry.endpoint` | OTLP gRPC endpoint (`--features otel`) | disabled |
| `telemetry.sample_ratio` | Fraction of normal requests to trace | `0.1` |

See [`config.example.toml`](config.example.toml) for a fully annotated reference configuration covering all providers, hook definitions, guardrails, policy rules, OIDC, and telemetry options.

### Model routing

Models resolve in this order:

1. Alias lookup from `routing.model_aliases`
2. Provider prefix — `anthropic/claude-opus-4-5` routes to the `anthropic` provider
3. Fall back to `routing.default_provider`

---

## Architecture

```
src/
├── api/
│   ├── admin/
│   │   ├── auth.rs             — JWT issuance and verification, AdminSession extractor
│   │   ├── dashboard.rs        — browser dashboard handlers (HTMX, mr_admin_session cookie)
│   │   ├── oidc.rs             — OIDC state store, PKCE, discovery, token validation, SSO handlers
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
│   ├── declarative_policy.rs   — TOML-configured policy rule matching (condition + allow-list + budget)
│   ├── policy.rs               — PolicyEngine: declarative rules (priority) then DB budget/rate rules
│   ├── engine.rs               — RequestRouter: alias resolution, provider selection, load balancing
│   └── ...                     — cache, circuit_breaker, fallback, retry, session_limits
├── report/                     — Cost reporting and audit log formatting
└── telemetry/                  — OTel init, SmartSampler, metrics instruments (--features otel)
```

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
