# Changelog

## [Unreleased]

### Security

- **`init` generates a real JWT signing secret.** It previously wrote the
  published placeholder `change-me-jwt-secret` into every fresh config. Combined
  with the startup guard below, this meant a fresh install could not start:
  `init` printed "Run: modelrouter serve" and `serve` refused. `init` now
  substitutes a 256-bit random secret at both write sites. Existing configs are
  untouched — `init` only writes when no config exists or you confirm an
  overwrite.
- **`serve` refuses to start on a weak `auth.jwt_secret`** — empty, or still the
  shipped placeholder — and names the field. (#48, PR #52)
- **Helm chart env-var names corrected.** Four entries used `MODELROUTER__X`
  where the loader expects `MODELROUTER_X__Y`, so they were silently discarded.
  The chart's ConfigMap ships `jwt_secret = ""` expecting the env var to
  override it, and it never did: every Helm deployment had been signing admin
  sessions with an empty secret. (#48, PR #52)
- **MCP server writes are scoped to their owner.** Any valid key could edit or
  delete any registration. Migration 028 adds an owner column; rows predating it
  are read-only through the API. (#49, PR #52)
- **The observability egress now honours `[storage] store_prompt_content`.**
  With content storage off (the default), the prompt row was redacted but the
  `CallbackEvent` sent to Langfuse, LangSmith or a webhook still carried the
  full prompt and response. Both are now redacted from one shared helper. (#53)

### Fixed

- **`serve` honours `[server] host`, `port` and `request_body_limit_mb`.** The
  `--host`/`--port` flags carried clap defaults that always won, so the config
  values were never read; `request_body_limit_mb` was never referenced at all,
  so axum's 2 MB default applied whatever the config said. Precedence is now
  flag > config > built-in default. **Behaviour change:** an operator with a
  `port` in config that differs from 8080, who has been getting 8080, now gets
  the configured port. (#55)
- **`/admin/webhooks` returned 500** — the template was never registered with
  the environment. A test now walks `templates/admin/` and fails on the next
  unregistered page. (#54)
- **`cargo test` did not build on `main`.** Two independent breakages had
  accumulated across eight test targets. 425 tests pass again. (PR #52)
- **Per-model parameter deprecations no longer take a provider down.** Vertex
  rejects `temperature` on Claude 5 with a 400; five of those opened the circuit
  breaker for every Vertex-backed model. A per-model capability table
  (`[[model_capabilities]]`, operator-overridable) strips parameters the
  resolved model is known to reject, and client errors no longer count toward
  the breaker. (#47)
- `chat_completions` responses include the resolved backing model. (#46)

### Added

- **End-to-end test tier** (`tests/test_e2e_*.rs`) that runs the compiled
  binary as a child process against a mock OpenAI-shaped provider. Covers
  `init`/`migrate`/`serve`, the startup guards, auth rejection before the
  upstream call, the cost ledger, the cache, and streaming. `#[ignore]`d by
  default; see [`docs/testing/e2e-harness.md`](docs/testing/e2e-harness.md).
  This is the tier that would have caught the `init` defect above: none of the
  in-process tests execute `serve`.
- **Intelligent model routing design spec** (revision 16) at
  `docs/superpowers/specs/2026-09-02-intelligent-model-routing-design.md`,
  covering plugin routers, tiered pools, learning, experiments (paired mirroring
  and application-level A/B runs), the `/admin/compare` page, and content
  retention under experiment. Documentation only — no routing code is
  implemented.

### Since 0.1.0, not previously itemised

This changelog was not maintained between 0.1.0 and the entries above. The
larger additions in that period, in merge order: session stickiness with a
model-change override; `X-No-Log`; DB-managed webhook callbacks with admin UI
and CLI; `:fastest`/`:cheapest` routing shortcuts; Chinese provider pricing
(DeepSeek, Qwen, Doubao); a DB-driven model registry with failover chains; the
Vertex AI provider, embedding and web-search adapters; the reports page with
burndown charts; response caching (memory or Redis) with `/health/deep`
capability probes; search proxying; per-request project attribution and
failure capture; the `[storage]` prompt-log policy with a dedicated
`prompt_db_path`; per-caller cache opt-out via policy rules; and provider
catalog discovery (`/admin/api/models/available`). See `git log v0.1.0..` for
the full record.

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
- Postgres database support via `--features postgres`
- `modelrouter install-service` / `uninstall-service` for macOS (launchd) and Linux (systemd)
- Docker image with distroless runtime
- Single static binary, no runtime dependencies
