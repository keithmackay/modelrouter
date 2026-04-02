# modelrouter

![Release](https://img.shields.io/github/actions/workflow/status/keithmackay/tokenomics/release.yml?label=release)
![Version](https://img.shields.io/badge/version-0.1.0-blue)
![License](https://img.shields.io/badge/license-MIT-green)
![Rust](https://img.shields.io/badge/rust-2021-orange)

An OpenAI-compatible LLM proxy that routes requests across providers, enforces per-user spend budgets, and runs configurable hooks — all from a single self-hosted binary.

Point your existing OpenAI SDK at modelrouter instead of `api.openai.com`. It authenticates your users with API keys, resolves model aliases, selects the right upstream provider, and tracks token spend — stopping requests that would blow a user's monthly budget before they hit the provider.

---

## Table of Contents

- [Highlights](#highlights)
- [Getting Started](#getting-started)
- [Usage](#usage)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Development](#development)
- [Contributing](#contributing)
- [License](#license)

---

## Highlights

- **Drop-in OpenAI compatibility** — any SDK that speaks `POST /v1/chat/completions` works without modification
- **Multi-provider routing** — route to OpenAI, Anthropic, Google Gemini, or Ollama; switch providers by changing one config line
- **Per-user budget enforcement** — set monthly or custom-window spend limits; over-budget requests are rejected before they reach the upstream
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

**From source:**

```bash
git clone https://github.com/keithmackay/tokenomics.git
cd tokenomics
cargo build --release
# Binary is at target/release/modelrouter
```

**Docker:**

```bash
docker build -t modelrouter .
docker run -v /host/config:/config -v /host/data:/data \
  -e MODELROUTER_CONFIG=/config/config.toml \
  -p 3000:3000 modelrouter serve
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
modelrouter budget set --user alice --limit 10.0 --window monthly

# Cost reporting
modelrouter report cost --user --window monthly --format table
modelrouter report cost --format csv > report.csv

# Install as a system service (macOS or Linux)
modelrouter install-service
```

### API

All `/v1/*` endpoints accept `Authorization: Bearer <api-key>`.

```bash
# List available models
curl http://localhost:3000/v1/models \
  -H "Authorization: Bearer <api-key>"

# Chat completion — identical to OpenAI API
curl http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "claude-opus-4-5",
    "messages": [{"role": "user", "content": "Hello"}]
  }'

# Health check
curl http://localhost:3000/health
```

Admin endpoints at `/admin/*` require a JWT from `POST /admin/login`. A browser-based dashboard is available at `/admin`.

---

## Configuration

Configuration lives at `~/.modelrouter/config.toml` by default, or at the path in `MODELROUTER_CONFIG`.

| Key | Description | Default |
|-----|-------------|---------|
| `server.host` | Bind address | `127.0.0.1` |
| `server.port` | Listen port | `3000` |
| `database.path` | SQLite file path | `~/.modelrouter/router.db` |
| `routing.default_provider` | Fallback provider when model prefix is absent | `openai` |
| `routing.model_aliases` | Map short names to canonical model IDs | — |
| `providers.<name>.api_key` | Upstream provider API key | required |
| `providers.<name>.base_url` | Override provider endpoint | provider default |
| `auth.admin_secret` | Secret for admin JWT signing | required |
| `telemetry.endpoint` | OTLP gRPC endpoint (`--features otel`) | disabled |
| `telemetry.sample_ratio` | Fraction of normal requests to trace | `0.1` |

See [`config.example.toml`](config.example.toml) for a fully annotated reference configuration covering all providers, hook definitions, and telemetry options.

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
│   ├── app.rs              — axum router assembly, middleware stack
│   └── routes/
│       ├── completions.rs  — POST /v1/chat/completions handler
│       ├── models.rs       — GET /v1/models handler
│       └── admin/          — admin REST + dashboard handlers
├── cli/                    — Clap CLI commands (serve, init, migrate, user, budget, report)
├── config/                 — Config loading and schema (TelemetryConfig, Settings, …)
├── db/                     — sqlx migrations and repository types
├── hooks/
│   ├── lifecycle.rs        — before/after request lifecycle hooks
│   └── pipeline.rs         — streaming pipeline hooks
├── router/
│   └── policy.rs           — budget and rate-limit policy engine
├── report/                 — cost reporting and audit log formatting
└── telemetry/              — OTel init, SmartSampler, metrics instruments (--features otel)
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
