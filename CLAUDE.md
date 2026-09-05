# modelrouter — Rust Implementation

## Quick Start
```bash
cargo build --release
```

## Development
```bash
cargo run -- init      # Create config
cargo run -- migrate   # Run migrations
cargo run -- serve     # Start server
```

## Testing
```bash
cargo test
cargo build --features postgres  # Verify postgres feature
cargo build --features bedrock  # Verify bedrock feature
```

## CLI Commands
```
modelrouter init
modelrouter migrate
modelrouter serve [--config <path>]
modelrouter user create --name alice
modelrouter user list
modelrouter budget set --user alice --limit 10.0 --window monthly
modelrouter report cost [--user] [--window] [--format table|csv|json]
modelrouter report compare --dimension model|provider|tag|run|variant [--key <tag-key|experiment-id>] --a <arm> --b <arm> [--window] [--format table|csv|json]
modelrouter experiment add --name <n> --variant <label>=<key>:<target>[,...] ... --expires-at <RFC3339|never> --content-retention-days <n> [--retain-content] [--feed-learning] [--allow-user <name>]...
modelrouter experiment list [--status active|closed|all] [--format table|csv|json]
modelrouter experiment close --id <id>
modelrouter experiment results --id <id> [--limit <1-1000>] [--offset <n>] [--format table|csv|json]
modelrouter webhook list
modelrouter webhook add --name <name> --url <url> [--events completion] [--secret-header-name <h>] [--secret-header-value <v>]
modelrouter webhook delete --id <id>
modelrouter webhook enable --id <id>
modelrouter webhook disable --id <id>
modelrouter install-service  (macOS/Linux)
```

## Key Paths

- Config: `~/.modelrouter/config.toml` (or `MODELROUTER_CONFIG` env var)
- Database: `~/.modelrouter/router.db` (configurable)
- Logs: stdout via tracing/tracing-subscriber

## API Endpoints

| Endpoint | Description |
|---|---|
| `GET /health` | Liveness check |
| `GET /v1/models` | List available models |
| `POST /v1/chat/completions` | Proxy chat completions (OpenAI-compatible); `x-modelrouter-experiment: <id>[:<label>]` binds the request to an experiment variant |
| `POST /v1/feedback` | Report a run's outcome by attribution correlation id (API key) |
| `GET /admin/users` | List users (admin JWT required) |
| `POST /admin/users` | Create user (superadmin JWT required) |
| `GET /admin/stats` | Usage stats (admin JWT required) |
| `GET /admin/budgets` | List budget rules (admin JWT required) |
| `GET /admin/audit` | Audit log (admin JWT required) |
| `GET /admin/api/compare` | Two-arm comparison by model, provider, tag, run or experiment variant (admin JWT required; see `docs/experiments.md`) |
| `GET /admin/compare` | Comparison page (admin dashboard) |
| `GET /admin/api/experiments` | List experiments, `?status=active\|closed\|all` (admin JWT required) |
| `POST /admin/api/experiments` | Create an experiment; expiry and retention required, every target must be priced (superadmin JWT required) |
| `GET /admin/api/experiments/:id` | One experiment (admin JWT required) |
| `POST /admin/api/experiments/:id/close` | Close an experiment (superadmin JWT required) |
| `GET /admin/api/experiments/:id/results` | Per-variant and per-run results, `?limit=&offset=` (admin JWT required) |
| `GET /admin/experiments` | Experiments page (admin dashboard); `POST` creates from the form (superadmin session) |
| `POST /admin/experiments/:id/close` | Close from the dashboard (superadmin session) |
| `GET /admin/experiments/:id/panels` | Results panels for one experiment (admin dashboard) |
| `GET /admin/webhooks` | Webhook management page (admin dashboard) |
| `GET /admin/api/webhooks` | List webhook backends (admin JWT required) |
| `POST /admin/api/webhooks` | Create webhook backend (superadmin JWT required) |
| `DELETE /admin/api/webhooks/:id` | Delete webhook backend (superadmin JWT required) |
| `POST /admin/api/webhooks/:id/enable` | Enable webhook (superadmin JWT required) |
| `POST /admin/api/webhooks/:id/disable` | Disable webhook (superadmin JWT required) |

## Authentication

All `/v1/*` endpoints require `Authorization: Bearer <api-key>`.

Admin REST endpoints require a JWT obtained from `POST /admin/login`.
Dashboard (web UI) at `/admin` uses cookie-based sessions.

## Model Routing

Models are resolved in this order:
1. Alias lookup from `routing.model_aliases` in config
2. Split on `/` — e.g. `anthropic/claude-opus-4-5` routes to the `anthropic` provider
3. Fall back to `routing.default_provider`

## Privacy: no downstream application names

modelrouter is public. Never name a specific application, customer, team or
organisation that uses it — in code, comments, commit messages, docs, specs,
migrations, tests or issues. That includes internal repo paths, private issue
numbers, and dated incident references that identify a deployment.

Write "the pilot application", "a downstream caller", "one production caller",
and describe the engineering fact the name was standing in for. The rationale
survives; the identity does not.

Before committing: `grep -rniI --exclude-dir=target <name> .` for any name you
touched. Git history is public too, so catch it before the commit, not after.

## Dev Conventions

- Use `cargo` for all Rust commands
- Logging via `tracing` / `tracing-subscriber`
- All DB operations async via `sqlx` + `aiosqlite`
- Migrations tracked via `sqlx::migrate!("./migrations")`
- API keys stored as SHA-256 hex digest only
- Hook capabilities are NOT auto-granted — operators must INSERT rows into `hook_permissions`
