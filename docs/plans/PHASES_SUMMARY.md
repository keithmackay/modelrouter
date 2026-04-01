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

## Future Enhancements (Post-Launch)

See the **Next Steps** section of `implementation-plan.md` for the full list with rationale. Top candidates:

1. **OIDC/SSO for admin auth** — `SessionProvider` trait is already designed for this [Keith's idea]
2. **Prometheus metrics endpoint** — `/metrics` for Grafana/alerting [Claude's idea]
3. **Budget alerts via webhook** — native Slack/webhook config without shell scripts [Keith's idea]
4. **Homebrew tap** — `brew install keithmackay/tap/modelrouter` [Keith's idea]
5. **Fallback chain execution** — retry on provider failure/timeout [Claude's idea]
