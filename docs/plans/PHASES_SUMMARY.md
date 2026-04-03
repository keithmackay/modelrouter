# modelrouter — Rust Rewrite: Phases Summary

_Quick reference for the full implementation plan in `implementation-plan.md`_
_Written: 2026-03-31_

---

## Technology Stack

| Layer | Choice |
|---|---|
| Language | Rust (edition 2021, MSRV 1.75) |
| HTTP server | axum 0.7 + tower |
| Async runtime | tokio (full) |
| Database | sqlx 0.8 — SQLite default, Postgres via `--features postgres` |
| Outbound HTTP | reqwest 0.12 (connection pooling per provider) |
| CLI | clap 4 (derive API) |
| Config | TOML via `config` crate, env var overrides (`MODELROUTER_*`) |
| Admin templates | minijinja (runtime, embedded in binary) + HTMX |
| Logging | tracing + tracing-subscriber |
| Auth | SHA-256 hashed API keys; bcrypt admin passwords; HMAC-SHA256 JWTs |
| Output formatting | comfy-table (CLI), serde_json (JSON mode) |

---

## Key Principles

- **TDD** — write the failing test first, always. No production code without a red test.
- **DRY** — one `Repository` trait per domain object; SQLite and Postgres are implementations, not duplicates.
- **YAGNI** — implement only what the current phase requires. Future phases are explicitly scoped.
- **Frequent commits** — commit after every task. A commit = a logical unit that compiles and passes tests.
- **Single binary** — everything ships in one `modelrouter` binary. No runtime, no venv, no separate processes.

---

## Phase Overview

### Phase 1 — Project Scaffold and Configuration
**Goal:** Compilable project, working config loading, CLI skeleton, `modelrouter init`.

| # | Task |
|---|---|
| 1.1 | Initialise Cargo project (`[lib]` + `[[bin]]`, all dependencies) |
| 1.2 | Config schema (`src/config/schema.rs`) — all serde structs with defaults |
| 1.3 | Config loader (`src/config/mod.rs`) — file + env var layering |
| 1.4 | Tests for config loading and env override |
| 1.5 | CLI skeleton (`src/cli/commands.rs`) — all subcommands as stubs |
| 1.6 | `modelrouter init` — create annotated config file from embedded template |
| 1.7 | `src/main.rs` (thin entry point into lib) |
| 1.8 | `config.example.toml` — fully annotated, user-facing documentation |

**Deliverable:** `cargo run -- init` creates a config file. All subcommands parse without panicking.

---

### Phase 2 — Database Layer
**Goal:** Full schema, all Repository traits, SQLite implementations, idempotent migrations.

| # | Task |
|---|---|
| 2.1 | `migrations/001_initial.sql` — complete schema (10 tables, indices) |
| 2.2 | Repository traits (`src/db/repositories/`) — one trait per domain, no sqlx in traits |
| 2.3 | Domain model structs (`src/db/models.rs`) — `FromRow` + `Serialize` |
| 2.4 | SQLite implementations (`src/db/sqlite/`) — parameterised queries via `sqlx::query_as!` |
| 2.5 | Migrations runner (`src/db/migrations.rs`) — `run_migrations()`, dev key warning |
| 2.6 | Dev seed (only when `MODELROUTER_DEV_SEED=true`) |
| 2.7 | Tests — idempotency, user CRUD, token rotation overlap window |

**Key decision:** `find_by_api_key` checks both `api_key` AND `api_key_old` (within expiry) to support zero-downtime token rotation.

**Deliverable:** `modelrouter migrate` runs cleanly. All repository operations covered by tests.

---

### Phase 3 — Core Proxy (MVP)
**Goal:** Working proxy. Auth, routing, provider adapters, streaming, cost logging. No budget enforcement yet.

| # | Task |
|---|---|
| 3.1 | Provider types — `NormalizedRequest`, `CompletionResult`, `ProviderAdapter` trait, `SseStream` |
| 3.2 | `OpenAICompatAdapter` — reqwest, streaming + non-streaming, configurable `api_base` |
| 3.3 | `AnthropicAdapter` — message translation, `x-api-key` auth, native Messages API |
| 3.4 | Provider registry — cached by `(provider, api_key, api_base)` |
| 3.5 | `CostCalculator` — hard-coded pricing table, strips provider prefix, unknown = $0 |
| 3.6 | `RequestRouter` — alias → prefix → default resolution order |
| 3.7 | API key auth extractor — SHA-256 hash, checks both current and rotation-overlap key |
| 3.8 | `AppState` + `DatabaseProvider` aggregating trait |
| 3.9 | `POST /v1/chat/completions` — stream + non-stream, fire-and-forget cost logging |
| 3.10 | `GET /v1/models`, `GET /health` |
| 3.11 | `ApiError` → axum `IntoResponse` with OpenAI-shaped error JSON + `request_id` |
| 3.12 | `modelrouter serve` command |
| 3.13 | Tests — routing, cost, completions (mock adapter), SSE text extraction |

**Critical fix from Python prototype:** SSE token estimation must extract only `choices[0].delta.content` text, not the full SSE envelope string.

**Deliverable:** `modelrouter serve` proxies requests to Anthropic and OpenAI. All prompts logged.

---

### Phase 4 — Budget Controls, Policy, and Admin Auth
**Goal:** Per-user budgets enforced. Named admin accounts with JWT sessions. Audit log. Token rotation.

| # | Task |
|---|---|
| 4.1 | `PolicyEngine` — model allow/deny, rate limit, budget window check |
| 4.2 | Wire policy into completions endpoint (check before provider dispatch) |
| 4.3 | Admin user model + bcrypt password hashing |
| 4.4 | Admin JWT middleware (bearer header for API, HttpOnly cookie for dashboard) |
| 4.5 | Audit log helper — explicit call at end of every admin mutation handler |
| 4.6 | Admin REST API — user CRUD, budget CRUD, stats, audit log |
| 4.7 | Token rotation — `rotate_key()`, overlap window, `expire_old_keys()` |
| 4.8 | CLI: `modelrouter user` and `modelrouter budget` subcommands |
| 4.9 | Tests — policy decisions, admin auth, token rotation, audit entries |

**Key decision:** Admin accounts are named DB rows with individual credentials — no shared admin token. Every action is traceable to a specific admin. Roles: `superadmin` (full access) and `viewer` (read-only).

**Deliverable:** Requests blocked when budget exceeded. Admin API fully functional.

---

### Phase 5 — Hook System
**Goal:** Lifecycle and pipeline hooks running, with operator-controlled permission grants.

| # | Task |
|---|---|
| 5.1 | Lifecycle hook runner — fire-and-forget tokio subprocess, timeout, event payloads |
| 5.2 | Pipeline hook runner — stdin/stdout JSON, timeout, `fail_open`/`fail_closed` |
| 5.3 | Permission checker — `hook_permissions` table is runtime source of truth |
| 5.4 | Wire hooks into completions endpoint (pre_request, post_response, lifecycle events) |
| 5.5 | Hook metrics — insert duration row after every execution for latency tracking |
| 5.6 | Tests — hook execution, timeout behaviour, permission enforcement |

**Key design:** A pipeline hook can *declare* `capabilities` in config, but those capabilities are only active if an operator has granted them in the `hook_permissions` table. The hook binary cannot self-grant permissions.

**Hook types:**
- **Lifecycle** (fire-and-forget): `on_request_received`, `on_response_sent`, `on_budget_exceeded`, `on_stream_complete`, `on_error`, `on_user_disabled`
- **Pipeline** (synchronous): `pre_request`, `post_response`

**Deliverable:** Hooks fire correctly. Permission revocation without restart works.

---

### Phase 6 — Reporting CLI
**Goal:** All `modelrouter report` subcommands produce tables, CSV, and JSON.

| # | Task |
|---|---|
| 6.1 | Report query layer — analytics queries (not CRUD) for cost, usage, prompts, hooks |
| 6.2 | Output formatter — `comfy-table` (human), CSV, JSON (stable, pipeable) |
| 6.3 | `modelrouter report cost [--user] [--window] [--format]` |
| 6.4 | `modelrouter report usage [--model] [--project] [--since]` |
| 6.5 | `modelrouter report prompts [--user] [--limit] [--since]` |
| 6.6 | `modelrouter report audit [--actor] [--tail]` |
| 6.7 | `modelrouter report hooks` (p50/p95/p99 per hook name) |
| 6.8 | Tests — query correctness, JSON validity, CSV headers |

**Deliverable:** All report commands work. JSON output is stable and pipeable into `jq`.

---

### Phase 7 — Admin Dashboard
**Goal:** `/admin` web UI with all key views, HTMX-powered partial updates, no JS framework.

| # | Task |
|---|---|
| 7.1 | minijinja environment — templates embedded via `include_str!` at compile time |
| 7.2 | Dashboard JWT middleware — reads cookie, redirects to login if missing/expired |
| 7.3 | Login (`GET/POST /admin/login`) and logout (`POST /admin/logout`) |
| 7.4 | Overview page — spend today/week/month, request count, budget warnings |
| 7.5 | Users page — table, create form, disable/enable, rotate key (HTMX row replacement) |
| 7.6 | Prompts page — paginated log, expandable rows |
| 7.7 | Cost breakdown page — by user, model, project, window |
| 7.8 | Hooks page — registered hooks, permissions, p50/p95/p99 latency |
| 7.9 | Audit log page — paginated, filterable |
| 7.10 | Admins page — admin account management (superadmin only) |
| 7.11 | Tests — auth redirect, page rendering, superadmin-only access |

**Pattern:** Actions (disable user, set budget) POST to `/admin/api/*` REST endpoints which return HTML fragments. HTMX swaps them in. No JavaScript state, no page reload.

**Deliverable:** Complete web dashboard accessible at `/admin`.

---

### Phase 8 — Deployment and Postgres
**Goal:** Multi-arch binary releases, Docker, service install, Postgres support, v0.1.0 tag.

| # | Task |
|---|---|
| 8.1 | GitHub Actions release workflow — 4-target matrix (Linux x86_64/arm64, macOS arm64/x86_64) |
| 8.2 | Dockerfile — distroless runtime, SQLite bundled, config/DB as volumes |
| 8.3 | `docker-compose.yml` example |
| 8.4 | `contrib/dev.modelrouter.plist` (macOS launchd) |
| 8.5 | `contrib/modelrouter.service` (Linux systemd) |
| 8.6 | `modelrouter install-service` / `uninstall-service` — detect platform, write and register |
| 8.7 | Postgres repository implementations (`src/db/postgres/`) behind `--features postgres` |
| 8.8 | `modelrouter init` polish — first-run experience, clear next-step instructions |
| 8.9 | CHANGELOG.md, tag `v0.1.0`, push |

**Deliverable:** Single-command install on macOS and Linux. Docker image published. v0.1.0 released.

---

### Phase 9 — OpenTelemetry Integration ✅ Complete
**Goal:** Full OTel observability (traces, metrics, logs) via OTLP/gRPC, behind `--features otel`.

| # | Task |
|---|---|
| 9.1 | `otel` feature flag + 5 optional OTel crate dependencies in `Cargo.toml` |
| 9.2 | `TelemetryConfig` schema (cfg-gated); `[telemetry]` section in `config.example.toml` |
| 9.3 | `SmartSampler` — parent propagation, `force_sample` attribute, ratio-based sampling |
| 9.4 | Metrics instruments — 7 instruments in `OnceLock<Instruments>` (requests, tokens, cost, latency, policy denials, hook duration) |
| 9.5 | `init_telemetry()` — three OTLP gRPC pipelines (BatchSpanProcessor, PeriodicReader 15s, BatchLogProcessor); layered tracing subscriber |
| 9.6 | `TelemetryShutdownGuard` — flushes all three pipelines on Drop |
| 9.7 | `TraceLayer` wired unconditionally on axum router |
| 9.8 | `chat_completions` span attributes (`user.id`, `model`, `provider`, `cost.usd`, `tokens.prompt`, child spans for policy check and provider call) |
| 9.9 | Hook spans (`#[instrument]` on pipeline and lifecycle hooks) |
| 9.10 | Policy span (`#[instrument]` on `PolicyEngine::check()`) |
| 9.11 | Tests — sampler unit tests, metrics recording, init/shutdown, span attribute coverage |

**Deliverable:** `cargo build --features otel` + a running OTLP collector (e.g. Arize Phoenix) produces traces, metrics, and logs. Default binary unchanged.

---

### Phase 10 — Quick Wins: Critical and High-Impact Low-Effort Features ✅ Complete
**Goal:** Five high-value features each achievable in a day or less, no new subsystems required.
_Source: LiteLLM gap analysis items 1–5. See `docs/2026-04-02-litellm-feature-gap.md`._

| # | Task |
|---|---|
| 10.1 | `POST /v1/messages` — Anthropic Messages API passthrough; unblocks Claude Code as a native client |
| 10.2 | Enforce TPM/RPM token limits in `PolicyEngine` — `limit_tokens` column already in schema |
| 10.3 | Config-driven pricing table — replace hardcoded map in `router/cost.rs` with `[[pricing]]` TOML config |
| 10.4 | Fallback chain retry loop — `FallbackChain::try_in_order()` over already-parsed config |
| 10.5 | `GET /metrics` Prometheus endpoint — thin wrapper over existing OTel metric instruments |
| 10.6 | Tests for all five features |

**Key decision:** The `/v1/messages` route reuses the existing `AuthenticatedUser` extractor and cost logging pipeline; the only new code is format translation between Anthropic Messages API shape and `NormalizedRequest`.

**Deliverable:** Claude Code can point `ANTHROPIC_BASE_URL` at modelrouter with no adapter. Token limits enforced. Provider pricing customisable. Prometheus scrape target live.

---

### Phase 11 — Core Expansion: High-Impact Medium-Effort Features
**Goal:** Caching, embeddings, per-key budgets, cloud provider adapters, and real load balancing.
_Source: LiteLLM gap analysis items 6–12._

| # | Task |
|---|---|
| 11.1 | Complexity router — cheap-model routing for simple prompts via heuristic token-count threshold |
| 11.2 | Exact-match response cache — Redis or in-memory LRU, keyed on (model, messages hash, params) |
| 11.3 | Semantic cache — optional Qdrant integration for similarity-based cache hits |
| 11.4 | `POST /v1/embeddings` — new route + provider adapters for OpenAI, Anthropic, and Bedrock Titan |
| 11.5 | Per-key budget entity — `api_keys` table, multiple keys per user, each with own `budget_rules` FK |
| 11.6 | Azure OpenAI provider adapter — model deployments, API version negotiation |
| 11.7 | AWS Bedrock adapter — SigV4 auth, Claude Converse API, Titan embeddings |
| 11.8 | Load balancer — round-robin, weighted, and latency-based strategies across a deployment pool |
| 11.9 | Tests for all features |

**Key decision:** Introduce a `Deployment` type (pool member) separate from `Provider`. A provider is a credential; a deployment is a routeable endpoint with health state and latency history.

**Deliverable:** Response caching reduces provider spend on repetitive workloads. Embeddings proxied. Azure and Bedrock unblocked. Load balanced across multiple API keys.

---

### Phase 12 — Provider Expansion and Polish: Medium-Impact Low-Effort Features
**Goal:** Extend provider coverage and add nine targeted quality-of-life improvements.
_Source: LiteLLM gap analysis items 13–21._

| # | Task |
|---|---|
| 12.1 | Provider adapters — Groq, Mistral, DeepSeek, OpenRouter (all OpenAI-compat; reuse existing adapter) |
| 12.2 | Circuit breaker — per-deployment failure state, cooldown window, auto-recovery |
| 12.3 | IP-based rate limiting — add source IP as an additional rate limit key dimension |
| 12.4 | Concurrent request limit — `max_parallel_requests` per user via per-user semaphore in `AppState` |
| 12.5 | Spend reset API — `POST /admin/api/spend/reset` zeros counters for a user or window |
| 12.6 | Per-tag budget rules — `tags` JSONB column on `api_keys`; budget rules can match by tag |
| 12.7 | Spend log cold storage — background job archives `cost_ledger` rows older than 90 days to S3 |
| 12.8 | Anthropic prompt caching — inject `cache_control` on long system prompts before upstream send |
| 12.9 | Key TTL and auto-rotation — `expires_at` on keys; background job rotates keys on schedule |
| 12.10 | Tests for all features |

**Deliverable:** Four new provider families available. Nine quality improvements shipped. No new subsystems introduced.

---

### Phase 13 — Enterprise Readiness: Medium-Impact Medium-Effort Features
**Goal:** Hot-reload, guardrails, LLM observability callbacks, session limits, K8s packaging.
_Source: LiteLLM gap analysis items 22–26 and 29._

| # | Task |
|---|---|
| 13.1 | Config hot-reload — background task polls DB every 10 s for new model deployments; applies without restart |
| 13.2 | Guardrail framework — `GuardrailHook` trait (pre/post call); first integration: Presidio PII detection |
| 13.3 | LangFuse callback — async span export via LangFuse SDK on request completion |
| 13.4 | LangSmith callback — same interface, LangSmith target |
| 13.5 | Session-based rate limits — `session_tpm_limit` / `session_rpm_limit` propagated via `X-Session-Id` header |
| 13.6 | `POST /v1/responses` — OpenAI Responses API passthrough |
| 13.7 | Kubernetes Helm chart — Deployment, Service, ConfigMap, HPA, liveness/readiness probes, PVC |
| 13.8 | Tests for all features |

**Deliverable:** modelrouter deployable on Kubernetes via Helm. LLM-specific observability platforms supported. PII protection available via guardrail config. Hot-reload eliminates restarts for model additions.

---

### Phase 14 — Advanced Platform: Medium/Low-Impact Higher-Effort Features
**Goal:** MCP support, declarative policy engine, batch API, queueing, and additional modalities.
_Source: LiteLLM gap analysis items 27–28 and 30–33._

| # | Task |
|---|---|
| 14.1 | MCP server registry — CRUD endpoints for MCP server definitions, per-key access groups |
| 14.2 | MCP tool calling — passthrough and routing for MCP tool invocations |
| 14.3 | Semantic MCP tool filtering — embedding-based top-K tool selection before context assembly |
| 14.4 | Declarative policy engine — condition-based rules with org→team→project→key cascade |
| 14.5 | Policy CRUD endpoints — `POST /admin/api/policies`, `GET`, `PUT`, `DELETE` |
| 14.6 | Batch API — `POST /v1/batches`, async job runner, result polling, post-completion cost tracking |
| 14.7 | Request queue — per-user async queue so rate-limited requests wait rather than immediately 429 |
| 14.8 | Image generation — `POST /v1/images/generations` with DALL-E and Stable Diffusion adapters |
| 14.9 | Audio — `POST /v1/audio/transcriptions` (Whisper) and `POST /v1/audio/speech` (TTS) |
| 14.10 | Tests for all features |

**Deliverable:** Claude Code MCP tool calls route through modelrouter with budget tracking and semantic filtering. Batch processing proxied. Image and audio modalities covered.

---

### Phase 15 — Enterprise Integrations: Low-Impact High-Effort Features
**Goal:** SSO, SCIM, billing hooks, shadow traffic, agent execution, vector stores, and realtime.
_Source: LiteLLM gap analysis items 34–40._

| # | Task |
|---|---|
| 15.1 | SSO / OIDC — admin dashboard login via Okta, Azure AD, or Auth0; PKCE flow |
| 15.2 | SCIM provisioning — sync users and groups from identity provider automatically |
| 15.3 | Shadow traffic routing — mirror a configurable fraction of requests to an alternate deployment |
| 15.4 | Billing integrations — push usage events to Stripe (metered billing) and Lago |
| 15.5 | Agent endpoints — `POST /agents`, `POST /agents/{id}/execute` with session memory and per-session budget |
| 15.6 | Vector store and RAG — `POST /v1/vector_stores`, file management, retrieval pipeline |
| 15.7 | Realtime WebSocket API — `GET /v1/realtime` WebSocket proxy for OpenAI Realtime API |
| 15.8 | Tests for all features |

**Deliverable:** Enterprise identity management automated. Billing systems connected. Full LiteLLM feature parity for all modalities and integration categories.

---

## Success Criteria

The implementation is complete when:

- [ ] `cargo build --release` produces a single static binary for all 4 targets
- [ ] `modelrouter init && modelrouter serve` works from zero on a fresh machine
- [ ] Anthropic and OpenAI requests proxy correctly with streaming
- [ ] Budget enforcement blocks over-limit requests with a 429
- [ ] Token rotation works with zero client downtime
- [ ] All lifecycle events fire hooks without blocking responses
- [ ] A pipeline hook with `mutate_request` capability can modify the request body
- [ ] A pipeline hook WITHOUT granted permission cannot mutate even if it tries
- [ ] `modelrouter report cost --format json` output is stable and parseable
- [ ] `/admin` dashboard loads, shows real data, all HTMX actions work
- [ ] `cargo test` passes with 0 failures
- [ ] Docker image runs correctly with config and DB mounted as volumes

---

## Post-Launch Maintenance

- **Pricing table** (`src/router/cost.rs`) — update when providers change prices. No schema change needed.
- **New providers** — implement `ProviderAdapter` trait + add a named section in `config.example.toml`. No other changes.
- **New hook events** — add event name to the lifecycle enum, fire it in the appropriate handler. Hook scripts pick up new events automatically.
- **Schema changes** — add a new migration file (`migrations/002_...sql`). `sqlx migrate` is idempotent; existing deployments auto-migrate on next startup.
- **Postgres support** — set `database.postgres_url` in config and rebuild with `--features postgres`. The same binary supports both; feature flag controls which pool is compiled in.

---

## Roadmap

Phases 10–15 are the structured improvement roadmap, derived from the LiteLLM feature gap analysis at `docs/2026-04-02-litellm-feature-gap.md`. Each phase is a self-contained sprint targeting one priority band from that analysis.

| Phase | Band | Items | Theme |
|-------|------|-------|-------|
| 10 | Critical + High / Low effort | 1–5 | Quick wins — maximum impact per hour |
| 11 | High / Medium effort | 6–12 | Core expansion — caching, embeddings, cloud providers, load balancing |
| 12 | Medium / Low effort | 13–21 | Provider breadth and polish |
| 13 | Medium / Medium effort | 22–26, 29 | Enterprise readiness — guardrails, K8s, hot-reload |
| 14 | Medium–Low / High effort | 27–28, 30–33 | Advanced platform — MCP, policy engine, new modalities |
| 15 | Low / High effort | 34–40 | Enterprise integrations — SSO, SCIM, billing, realtime |

Additional items not in the gap analysis:
- **Homebrew tap** — `brew install keithmackay/tap/modelrouter` [Keith]
- **WebAssembly plugin hooks** — replace shell subprocess hooks with `.wasm` modules via `wasmtime` [Claude]
- **Prompt advisor** — meta-LLM background worker that annotates stored prompts with improvement suggestions [Keith]
