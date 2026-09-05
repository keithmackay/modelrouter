# Changelog

## Unreleased

Everything since 0.1.0, by week. Entries are the merged result, not every
commit; `git log v0.1.0..` has the detail.

### Week of 2026-08-31

**Features**
- Controlled experiments (design spec §7a/§7c). Create an experiment with 2–16 named variants, each an overlay from a requested model name to a pinned, priced `provider/model`; expiry and content retention are required with no default, and creation is refused for any target that is a pool, would fall through to the default model, names an unconfigured provider or has no pricing entry. `x-modelrouter-experiment: <id>[:<label>]` on `/v1/chat/completions` binds the request to a variant — explicit, or assigned by a stable hash of `session_id` — with no downgrade, pool, affinity, cache or fallback; an unknown experiment or variant, a missing correlation id, or the header on any other endpoint is a 400. `POST /v1/feedback` records a run's outcome (`success`/`failure`, score, rating, note) by correlation id under the caller's key. `GET /admin/api/experiments/:id/results` returns per-variant and per-run cost, tokens, turns, span, latency, failures and outcomes in one paged document, also rendered by `modelrouter experiment results` and the `/admin/experiments` page; `/admin/compare` gains a `variant` dimension. Management via `/admin/api/experiments`, the dashboard and `modelrouter experiment add|list|close`; expired experiments auto-close within 60 s; a superadmin can have an experiment retain full prompt content for its own traffic, redacted in place `content_retention_days` after close. `docs/experiments.md` Part 1 is the client guide.

**Security**
- `init` generates a real JWT secret instead of writing the published placeholder, so a fresh install can start again.
- `serve` refuses to start on an empty or placeholder `auth.jwt_secret`.
- Helm chart env-var names corrected; deployments had been signing admin sessions with an empty secret. (#48)
- MCP server registrations can only be edited or deleted by their owner. (#49)
- Langfuse/LangSmith/webhook callbacks now honour `store_prompt_content`; they no longer receive full prompts when content storage is off. (#53)

**Features**
- End-to-end test tier that runs the real binary against a mock provider — startup, auth, routing, ledger, cache, streaming. (`docs/testing/e2e-harness.md`)
- Intelligent model routing design spec, revision 16 — plugin routers, tiered pools, experiments, `/admin/compare`. Design only.
- Chat completion responses include the resolved backing model. (#46)
- `/admin/compare`, `GET /admin/api/compare` and `modelrouter report compare`: compare two arms — tag values, correlation ids, models or providers — on cost, tokens, latency percentiles, cache hits and failures, with per-arm coverage and unpriced-model flags. `docs/experiments.md` shows a client application how to run and read an experiment.

**Fixes**
- `serve` honours `[server] host`, `port` and `request_body_limit_mb` from config; flags override for a single run. Operators with a non-default port in config now get it. (#55)
- `/admin/webhooks` rendered a 500 — template was never registered. (#54)
- Per-model parameter deprecations (e.g. `temperature` on Claude 5 via Vertex) are routed around instead of failing, and client errors no longer trip the provider circuit breaker. (#47)
- `cargo test` builds on `main` again; eight targets had rotted.

### Week of 2026-08-10

**Features**
- Redis-backed response cache with reconnect resilience; cache block on `/health` and `/health/deep` capability probes. (#22)
- `[storage]` prompt-log policy — store rows, content, and retention, editable from the admin UI. (#28, #36)
- Dedicated `prompt_db_path` so the prompt log can live in its own SQLite file. (#36)
- Per-caller response-cache opt-out via policy rules. (#37)
- Provider catalog discovery for Vertex, OpenAI-compatible and Anthropic, aggregated at `/admin/api/models/available`. (#38, #39, #40)
- Vertex AI embedding and web-search adapters.
- Runtime model aliases and model/provider disable from the admin UI. (#15)
- Per-request project override for cost attribution; small costs display legibly. (#16)
- Failed requests are captured and shown in the admin UI; silent model substitution removed; embeddings hardened. (#17)

**Fixes**
- `/v1/models` lists routing aliases with `alias_for`. (#31)
- A provider whose cargo feature is compiled out now fails loudly instead of silently. (#26)

### Week of 2026-08-03

**Features**
- Provider prompt-cache tokens are tracked and priced.
- Web search proxying with per-engine metering, Tavily first. (#11)
- Response caching for LLM and search calls, with hit rate in usage metrics. (#12)
- Per-request cost attribution (`x-attribution-*` headers) and an attribution-filtered usage query. (#14)

### Week of 2026-07-27

**Fixes**
- Group-membership index creation moved out of an already-applied migration.

### Week of 2026-06-15

**Features**
- Session stickiness: requests in the same session stay on the same model, with a model-change override and a per-key session window.
- `X-No-Log` header skips prompt logging while keeping cost tracking.
- DB-managed webhook callbacks with admin UI and CLI (SQLite and Postgres).
- `:fastest` / `:cheapest` routing shortcuts via `[routing.shortcuts]`.
- Chinese provider docs and pricing: DeepSeek, Qwen, Doubao.
- Mailto link on key rotation.

### Week of 2026-04-20

**Features**
- GCP Vertex AI provider — Gemini and Claude-on-Vertex. (#6)

### Week of 2026-04-13

**Features**
- Reports page with spend detail tables and budget burndown charts.
- Cost breakdown: all-time window, project/group/key/model filters, per-row detail; CLI cost report aligned with the UI.
- DB-driven model registry with failover chains. (#2)
- Zscaler / corporate CA support and a `check-tls` command.
- Mailto button after API key creation. (#1)

**Fixes**
- Router fallback bug.
- Burndown chart starts at the budget ceiling and shows remaining budget.
- Currency formatting, token in/out labels, and date sorting across admin tables.

### Week of 2026-04-06

**Features**
- CI publishes multi-arch Docker images to GHCR on release; four image variants (base, otel, postgres, full).
- `modelrouter admin` CLI for creating and managing admin users.
- `modelrouter key` CLI (create / list / rotate / disable) and `group` CLI.
- `report usage` CLI with scope, window and granularity flags.
- Per-project cost tracking via keys; `--user` / `--project` / `--group` filters on `report cost`.
- Keys & Users admin pages rebuilt: create-user form, copy-to-clipboard on new keys, duplicate-key guard, key history.
- Groups: tables, admin page, inline priority editing.
- Budgets: project / global scopes, total-window support, card-per-scope admin page.
- OTel docker-compose stack (Arize Phoenix).
- `init` sets 0700/0600 on the config directory and file.

**Fixes**
- Postgres repositories synced with SQLite (missing fields, `reset_spend`).
- HTMX bundled locally rather than loaded from a CDN.
- XSS escaping in group and budget cards.
- Debian-slim runtime image; offline vendor builds.

### Week of 2026-03-30

**Features**
- Anthropic Messages API passthrough (`/v1/messages`), plus `/v1/responses`, `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/speech` and `/v1/audio/transcriptions`.
- Providers: Azure OpenAI, AWS Bedrock (`--features bedrock`).
- Routing: complexity router (auto-downgrade on large prompts), fallback chains with retry, round-robin and weighted load balancing.
- Reliability: per-provider circuit breaker, transparent retry with backoff on 429/5xx, IP rate limiting, per-session TPM/RPM limits, per-user concurrency limits.
- Budgets: per-key budgets, per-tag budgets, token limits, spend reset, key expiry.
- Config-driven pricing table; config hot-reload every 30s.
- Declarative `[[policy_rules]]` matched by project/group/user/model.
- Guardrail framework with OpenAI moderation built in.
- OIDC SSO for admin login with PKCE.
- MCP server registry with CRUD endpoints and similarity-ranked discovery.
- Observability: Prometheus `/metrics` (`--features prometheus`), LangFuse and LangSmith callbacks, OTel design and implementation.
- Exact-match response cache with LRU + TTL.
- Cold-storage archival of cost rows to S3-compatible storage.
- Helm chart.

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
