# Changelog

## [0.1.0] - 2026-03-31

### Added
- Full OpenAI-compatible proxy (`/v1/chat/completions`, `/v1/models`)
- Streaming and non-streaming response support
- Anthropic and OpenAI provider adapters
- Per-user budget enforcement (daily/weekly/monthly windows)
- Model allow/deny policy per user
- Rate limiting (RPM)
- Lifecycle hooks (fire-and-forget subprocess)
- Pipeline hooks (synchronous stdin/stdout JSON mutation)
- Hook permission system (operator-controlled capability grants)
- Admin REST API with JWT authentication
- Admin web dashboard at `/admin` with HTMX-powered UI
- CLI reporting: cost, usage, prompts, audit log, hook latency (table/CSV/JSON)
- Zero-downtime API key rotation with overlap window
- SQLite default database with idempotent migrations
- Single static binary, no runtime dependencies
