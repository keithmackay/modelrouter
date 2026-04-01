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
| `POST /v1/chat/completions` | Proxy chat completions (OpenAI-compatible) |
| `GET /admin/users` | List users (admin JWT required) |
| `POST /admin/users` | Create user (superadmin JWT required) |
| `GET /admin/stats` | Usage stats (admin JWT required) |
| `GET /admin/budgets` | List budget rules (admin JWT required) |
| `GET /admin/audit` | Audit log (admin JWT required) |

## Authentication

All `/v1/*` endpoints require `Authorization: Bearer <api-key>`.

Admin REST endpoints require a JWT obtained from `POST /admin/login`.
Dashboard (web UI) at `/admin` uses cookie-based sessions.

## Model Routing

Models are resolved in this order:
1. Alias lookup from `routing.model_aliases` in config
2. Split on `/` — e.g. `anthropic/claude-opus-4-5` routes to the `anthropic` provider
3. Fall back to `routing.default_provider`

## Dev Conventions

- Use `cargo` for all Rust commands
- Logging via `tracing` / `tracing-subscriber`
- All DB operations async via `sqlx` + `aiosqlite`
- Migrations tracked via `sqlx::migrate!("./migrations")`
- API keys stored as SHA-256 hex digest only
- Hook capabilities are NOT auto-granted — operators must INSERT rows into `hook_permissions`
