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
- **Multi-scope budget enforcement** — set monthly or fixed date-range limits at the global (org-wide), project, user, or group level; any limit hit blocks the request before it reaches the upstream
- **Admin dashboard** — web UI at `/admin` with usage stats, audit log, and full management pages for users, API keys, groups, and budgets
- **Declarative policy engine** — TOML-configured rules that match users by project, group, or ID and enforce model allow-lists and budgets without touching the database
- **Content guardrails** — pluggable safety layer runs OpenAI moderation (or a custom HTTP endpoint) on requests and responses; configurable fail-open/fail-closed
- **MCP server registry** — register and discover Model Context Protocol servers via REST; semantic search ranks results by relevance to a query
- **SSO / OIDC** — admin users can authenticate via Google, Okta, Auth0, or any OIDC provider using authorization code flow with PKCE; new admins are auto-provisioned from email allow-lists
- **Hook system** — run shell scripts or HTTP webhooks at lifecycle events and in the request pipeline; grant capabilities per-user via `hook_permissions`
- **Feature-flagged optional components** — `--features postgres` for Postgres backend, `--features otel` for full OpenTelemetry observability (traces, metrics, logs via OTLP)
- **Single static binary** — SQLite bundled, no runtime dependencies; ships as a distroless Docker image

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

Create users via the CLI or the **Users** page in the admin dashboard.

**CLI:**

```bash
modelrouter user create --name abdoul
# Created user 'abdoul' (id=1)
# API key: mr-a1b2c3d4e5f6...
# Store this key securely — it cannot be retrieved later.

modelrouter user create --name becky
# Created user 'becky' (id=2)
# API key: mr-9z8y7x6w5v4u...
```

Each user gets a default API key at creation. Keys are shown exactly once — save them before closing the terminal.

```bash
modelrouter user list
#    1  abdoul  enabled
#    2  becky   enabled
```

### 5. Create projects and issue per-project keys

A **project** is a label applied to API keys. Every request made with a project key is attributed to that project in the cost ledger, enabling per-project spend reports and budget enforcement.

Create project keys via the **API Keys** page in the admin dashboard, or via the API (requires a superadmin JWT from `POST /admin/api/login`):

```bash
# Issue Abdoul a key for the "modelrouter" project
curl -s http://localhost:8080/admin/api/users/1/keys \
  -H "Authorization: Bearer <admin-jwt>" \
  -H "Content-Type: application/json" \
  -d '{"label": "modelrouter dev — abdoul", "project": "modelrouter"}'
# → {"id":3,"key":"mr-xxxx...","label":"modelrouter dev — abdoul","project":"modelrouter"}
# Save the key — it cannot be retrieved later.

# Issue Becky a key for the same project
curl -s http://localhost:8080/admin/api/users/2/keys \
  -H "Authorization: Bearer <admin-jwt>" \
  -H "Content-Type: application/json" \
  -d '{"label": "modelrouter dev — becky", "project": "modelrouter"}'
```

Share each key with the corresponding developer. See [Developer Setup](#developer-setup) for how developers add these to their environment.

### 6. (Optional) Create groups

Groups collect users for spend tracking and reporting. A user can belong to multiple groups; spend is attributed to their highest-priority group.

Go to **Admin → Groups** to create a group and add members. Groups are managed entirely in the web UI — there is no CLI command for group creation.

1. Click **Create Group**, enter a name (e.g. `team-alpha`) and priority (default 0; higher number = higher priority when a user belongs to multiple groups).
2. On the group card, use the **Add Member** dropdown to add Abdoul and Becky.

Once members are added, cost reports can be filtered by group:

```bash
modelrouter report cost --group team-alpha --window monthly
```

### 7. Configure budgets

Go to **Admin → Budgets** to set spend limits. Budgets are enforced independently — a request is blocked when *any* applicable rule is exceeded.

The Budgets page has four tabs:

**Global** — applies to all traffic org-wide. Use this as a hard ceiling on total provider spend.

- Example: add a **Monthly** rule with a $500 USD limit to cut off all requests once the org hits $500 for the month.
- Example: add a **Total** rule with a date range of `2026-04-01` → `2026-06-30` to cap spend for a fiscal quarter.

**Projects** — one card per project. Any project with an API key or an existing rule appears here.

- Example: add a **Monthly** $200 limit on the `modelrouter` project to cap all requests made with that project's keys.

**Users** — one card per user. Set per-developer monthly or total limits.

- Example: give Abdoul a **Monthly** $50 limit and Becky a **Monthly** $100 limit.

**Groups** — informational spend targets, not hard limits. These are tracked but never block a request.

- Example: set a **Target** of $300 for `team-alpha` to track aggregate group spend without blocking any individual user.

When a user hits their user limit, all their keys return `429 Budget exceeded` until the next monthly period. When a project or global limit is hit, all keys associated with that project (or all keys, for global) are blocked until the limit resets or is raised.

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

---

## Usage

### CLI

```bash
# User management
modelrouter user create --name alice
modelrouter user list

# Budget management (basic — see Admin → Budgets for full scope options)
modelrouter budget set --user alice --limit-usd 10.0 --window monthly

# Cost reporting
modelrouter report cost --user alice --window monthly --format table
modelrouter report cost --group team-alpha --window monthly
modelrouter report cost --project modelrouter --window monthly --format csv > report.csv

# Install as a system service (macOS or Linux)
modelrouter install-service
```

**Cost report filter matrix** — `--user` and `--group` are mutually exclusive; `--project` composes freely with either:

| Command | What it shows |
|---|---|
| `report cost` | All users, all projects |
| `report cost --user alice` | Alice across all her projects |
| `report cost --group team-alpha` | All users in group, all projects |
| `report cost --project modelrouter` | All users on the `modelrouter` project |
| `report cost --user alice --project modelrouter` | Alice on `modelrouter` only |
| `report cost --group team-alpha --project modelrouter` | Entire group on `modelrouter` only |

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

### OTel / Arize Phoenix

[Arize Phoenix](https://docs.arize.com/phoenix) is an open-source LLM observability platform. Start it locally:

```bash
pip install arize-phoenix && phoenix serve
# Phoenix UI: http://localhost:6006   OTLP gRPC: localhost:4317
```

Add to `config.toml` (requires `--features otel` build):

```toml
[telemetry]
enabled = true
endpoint = "http://localhost:4317"
service_name = "modelrouter"
sample_ratio = 1.0        # trace everything during initial setup
slow_threshold_ms = 2000  # always trace slow requests
```

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
