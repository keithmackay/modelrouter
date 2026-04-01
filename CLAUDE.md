# modelrouter — Project Instructions

## Quick Start

```bash
# Install dependencies
cd ~/Projects/modelrouter
uv sync

# Run DB migrations (creates ~/.modelrouter/router.db)
uv run modelrouter migrate

# Start the server
uv run modelrouter serve

# Or with a custom config
MODELROUTER_CONFIG=./config.yaml uv run modelrouter serve
```

## Entry Points

- CLI: `modelrouter` (entry point → `modelrouter.cli.commands:app`)
- Server: `modelrouter serve` — starts uvicorn on port 8080
- Migrations: `modelrouter migrate` — idempotent, safe to re-run
- User management: `modelrouter user create --name alice`, `modelrouter user list`

## Key Paths

- Config: `~/.modelrouter/config.yaml` (or `MODELROUTER_CONFIG` env var)
- Database: `~/.modelrouter/router.db` (configurable)
- Logs: stdout via loguru

## API Endpoints

| Endpoint | Description |
|---|---|
| `GET /health` | Liveness check |
| `GET /v1/models` | List available models |
| `POST /v1/chat/completions` | Proxy chat completions (OpenAI-compatible) |
| `GET /admin/users` | List users (admin token required) |
| `POST /admin/users` | Create user (admin token required) |
| `GET /admin/stats` | Usage stats (admin token required) |

## Authentication

All `/v1/*` endpoints require `Authorization: Bearer <api-key>`.

Dev key (seeded on first migration): `mr-dev-key`

Admin endpoints require the `admin_token` from config (default: `change-me-admin-token`).

## Model Routing

Models are resolved in this order:
1. Alias lookup from `routing.model_aliases` in config
2. Split on `/` — e.g. `anthropic/claude-opus-4-5` routes to the `anthropic` provider
3. Fall back to `routing.default_provider`

## Dev Conventions

- Use `uv run` for all Python commands (not `python3` directly)
- Logging via `loguru` (not stdlib `logging`)
- All DB operations async via `aiosqlite`
- Never import `litellm`
- API keys stored as SHA-256 hex digest only
