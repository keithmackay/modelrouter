# Changelog

## Unreleased

Everything since 0.1.0, by week. Entries are the merged result, not every
commit; `git log v0.1.0..` has the detail.

### Week of 2026-10-05

**Features**
- Claude on Azure AI Foundry: `[providers.foundry] anthropic_deployments = [...]` sends those deployments to Foundry's Anthropic Messages surface (`{resource}/anthropic/v1/messages`, `anthropic-version: 2023-06-01`) with the provider's credential (Entra bearer token for `https://ai.azure.com/.default` unless `entra_scope` is set, or `x-api-key`). Requests get the direct Anthropic translation: tools, thinking and effort, prompt caching, and streamed events translated to OpenAI chunks. Capability rules and pricing key on the deployment name, so name deployments after their model ids. Other deployments stay on the OpenAI-shaped surface.
- `[gateway.bootstrap]` (env `MODELROUTER_GATEWAY__BOOTSTRAP__USER` and `..._KEY` or `..._KEY_HASH`) creates a user and an API key at start-up if absent, so a fresh install serves a client whose key was provisioned outside the router with no manual `user create`. Idempotent: a disabled key stays disabled, rotation adds the new key without revoking the old one, and a key already held by another user refuses to start. The raw key is redacted from `Debug` and never serialized or logged.
- `[logging] format = "json"` (env `MODELROUTER_LOGGING__FORMAT=json`) writes every log line as one JSON object, with the event's fields flattened beside `timestamp`, `level` and `target`, so log collectors can query by field. The default stays `text`. Applies with and without the `otel` feature.
- `cache.require_opt_in` (default `false`): when `true`, only requests that send `x-modelrouter-cache: use` (or `refresh`) are cached; a request without the header is neither looked up nor stored, whatever its default eligibility. The stats API and `cache stats` report `require_opt_in` and `skipped_without_opt_in`. Combining it with `allow_header_opt_in = false` is refused at start-up.

**Fixes**
- A non-streamed `/v1/chat/completions` or `/v1/search` response the cache was not involved in no longer carries `x-modelrouter-cache: MISS`; it carries no cache header, as streamed and `/v1/messages` responses already did. `MISS` now always means the cache was consulted and held no entry.

### Week of 2026-09-28

**Features**
- Streamed completions are cacheable. A stream that finishes cleanly on `/v1/chat/completions` or `/v1/messages` is stored under the same key as the plain request; a hit on a `stream: true` request is replayed as SSE in the endpoint's wire format (chat chunks plus the usage-and-cost chunk and `[DONE]`, or the Anthropic `message_start` … `message_stop` sequence with final usage). Streamed and plain requests hit each other's entries. Errored, client-aborted and usage-less streams are not stored. `/v1/messages` joins the response cache for the first time, streamed or not, under the chat-completions eligibility rules.
- `x-modelrouter-cache: use|bypass|refresh` request header on `/v1/chat/completions`, `/v1/messages` and `/v1/search`. `use` caches a request the default rules would skip (e.g. a sampled temperature), `bypass` neither reads nor writes the cache, and `refresh` skips the lookup and stores the fresh answer. The response header reports `HIT`, `MISS`, `BYPASS` or `REFRESH`; an unknown value is a 400. `cache.allow_header_opt_in = false` makes the router ignore `use` and `refresh`. Requests without the header behave as before.
- `x-modelrouter-cache-ttl: <seconds>` request header sets the lifetime of the entry a request stores on the three cached endpoints; `0` asks for no expiry. Capped by the new `cache.max_ttl_seconds` (default one day; `0` lifts the cap). Unlimited entries are written to Redis without `EX`; the README covers the persistence and `maxmemory` settings they need. A malformed value is a 400.
- `x-modelrouter-cache-namespace: <name>` request header (1-64 characters from `A-Z a-z 0-9 . _ -`, otherwise a 400) labels the entries a request stores and the hits and misses it scores on the three cached endpoints. The label is not part of the key: the same prompt or query hits the same entry from any namespace. `[cache.namespaces.<name>] ttl_seconds` sets a namespace's default TTL (`0` = no expiry), below the TTL header. Per-namespace hits, misses, stores and savings appear under `live.by_namespace` in the stats API, `cache stats` and `/admin/cache` (first 256 namespaces, the rest under `*`). Purge one namespace with `POST /admin/api/cache/purge {"scope":"namespace",…}`, `modelrouter cache purge --namespace <name>` or the dashboard.
- Cost views list an uncached cost (`cost_usd + saved_usd`) beside the paid cost, so reruns of an identical workload compare at the same price instead of the last run looking cheapest because the cache served it. `uncached_cost_usd` is added to response `usage` and `x_router.cost`, `/admin/api/compare` (plus `uncached_cost_per_request` and their deltas; the compare page charts uncached cost), experiment results, attributed-usage totals, and the `report cost`, `report usage`, `report compare`, `experiment results` and attribution CLI output; the dashboard's cost, reports, compare, attribution and experiment pages gain an "Uncached" column. `report usage --format csv` gains `saved_usd` and `uncached_cost_usd` columns, and `report cost` gains "Uncached (USD)" and "Saved (USD)". Budgets and the overview page are unchanged and stay on paid cost.

**Fixes**
- The Anthropic adapter no longer drops an SSE event whose line is split across two network reads; streamed content and usage survive any chunking.
- `temperature` is no longer forwarded to Claude Opus 5.5, Sonnet 5.5, Opus 4.7 or 4.8, all of which reject it with a 400. The built-in temperature table now matches by model family, so point releases (`claude-opus-5-5`) and `@version` pins inherit their family's entry, and capability lookup strips every provider segment (`vertex/anthropic/claude-opus-5-5` reaches the same entry as `claude-opus-5-5`). Sonnet 5.5 is also listed as thinking by default, accepting effort, and unable to disable thinking.
- The router learns a model's `temperature` rejection. When a provider answers a request with a 400 that names `temperature`, the router retries it once without the parameter, records the exact model id (version included) in the new `learned_model_capabilities` table, logs one warning, and never sends `temperature` to that model again, across restarts. `GET /admin/api/model-capabilities/learned` lists entries with when they were learned, the error that taught them and how many requests each has changed since start; `DELETE /admin/api/model-capabilities/learned/:model` clears one. A `[[model_capabilities]]` config entry outranks a learned entry, which outranks the built-in table. Other 400s teach nothing.
- Built-in pricing for Claude Opus 5.5 ($4 / $20 per MTok, cache read $0.20, cache write $5) and Sonnet 5.5 ($2 / $10, cache read $0.20, cache write $2.50); Haiku 4.5 corrected from $0.80 / $4 to its $1 / $5 list price.

### Week of 2026-09-21

**Fixes**
- Slow LLM calls are no longer cut by the router. `[tier_timeouts]` defaults rise from 120s/600s/1800s to 2h/4h/6h (`fast`/`balanced`/`deep`), and the flat per-provider `timeout_secs` default from 1800s to 21600s. These are total-duration bounds that include a streamed body, so the old values killed healthy streams at 2, 10 or 30 minutes. The Vertex and Azure AI Foundry adapters now honour `[tier_timeouts]` per request instead of only their flat `timeout_secs`. A deployment that set `[tier_timeouts]` explicitly keeps its values.

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
- `/admin/compare`, `GET /admin/api/compare` and `modelrouter report compare`: compare two arms — tag values, correlation ids, models or providers — on cost, tokens, latency percentiles, cache hits and failures, with per-arm coverage and unpriced-model flags. `docs/experiments.md` shows a client application how to run and read an experiment. Follow-up: the unpriced flag also catches rows recorded before a price existed (tokens with zero spend), CSV output carries the latency-sample and unpriced rows, and arm values are capped at 256 characters.

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
