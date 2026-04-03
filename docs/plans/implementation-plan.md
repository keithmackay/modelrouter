# modelrouter — Rust Implementation Plan

_Written: 2026-03-31_
_Based on: working Python prototype (Phase 1 complete, with test suite)_

---

## Context for the Engineer

You are building `modelrouter` from scratch in Rust. There is an existing Python implementation in this repo that is a correct, tested reference. Read it. When in doubt about what a feature should do, the Python code is authoritative. This plan tells you how to build the Rust version — better, safer, and more deployable.

**What modelrouter does:** It is a self-hosted proxy that speaks the OpenAI `/v1/chat/completions` API. Clients (Cursor, VS Code, any tool using the `openai` SDK) point at it instead of OpenAI. It authenticates callers, enforces per-user spending budgets, logs every prompt and its cost, and forwards the request to the real provider (Anthropic, OpenAI, Gemini, Ollama, or any OpenAI-compatible backend). It streams responses back with zero added latency on the hot path.

**Why Rust:** This is a proxy — almost entirely I/O. Tokio + axum handles thousands of concurrent streaming connections with ~8MB idle memory vs ~100MB for Python. A single `cargo build --release` produces a self-contained binary with no runtime dependencies. SQLx gives compile-time checked SQL queries. The strong type system catches whole classes of bugs the Python version discovered at runtime.

---

## Guiding Principles

- **TDD**: Write the test first. Watch it fail. Write minimal code to pass it. Never write production code without a failing test that justifies it.
- **DRY**: Business logic lives once. Repositories are traits — SQLite and Postgres are just implementations of the same interface.
- **YAGNI**: Build what the phase requires. Do not add OIDC, WebSockets, or plugin hot-reload until their phase. Future phases are clearly marked.
- **Frequent commits**: Commit after every task, not every phase. A commit should be a logical unit of work that compiles and passes tests.
- **Naming**: Use the same domain vocabulary as the Python version — `User`, `BudgetRule`, `Prompt`, `CostLedger`, `Session`. Consistency with the reference makes code review easier.

---

## Tech Stack

| Concern | Crate | Notes |
|---|---|---|
| HTTP server | `axum` 0.7 | Tower middleware, built-in SSE support |
| Async runtime | `tokio` (full features) | |
| Database | `sqlx` 0.8 + SQLite | Compile-time checked queries |
| Outbound HTTP | `reqwest` 0.12 | Connection pooling per provider |
| CLI | `clap` 4 (derive API) | |
| Config | `config` crate + TOML | |
| Templates | `minijinja` | Runtime rendering, no codegen |
| Logging | `tracing` + `tracing-subscriber` | Structured, JSON-compatible |
| JSON | `serde` + `serde_json` | |
| Password hashing | `bcrypt` | Admin account passwords |
| JWT | `jsonwebtoken` | Admin dashboard sessions |
| Table output | `comfy-table` | CLI report formatting |
| Error handling | `thiserror` + `anyhow` | `thiserror` for library errors, `anyhow` in binaries |
| Testing | `tokio::test`, `axum-test` | |

---

## Directory Structure

```
modelrouter/
├── Cargo.toml
├── Cargo.lock
├── config.example.toml          # annotated example config
├── migrations/
│   └── 001_initial.sql          # full schema, idempotent
├── templates/
│   └── admin/
│       ├── base.html
│       ├── login.html
│       ├── overview.html
│       ├── users.html
│       ├── prompts.html
│       ├── cost.html
│       ├── hooks.html
│       ├── audit.html
│       └── admins.html
├── contrib/
│   ├── dev.modelrouter.plist    # macOS launchd
│   └── modelrouter.service      # Linux systemd
├── src/
│   ├── main.rs                  # binary entry point
│   ├── lib.rs                   # re-exports for integration tests
│   ├── config/
│   │   ├── mod.rs
│   │   └── schema.rs            # serde structs for config.toml
│   ├── db/
│   │   ├── mod.rs
│   │   ├── migrations.rs        # run_migrations(), idempotent
│   │   ├── repositories/
│   │   │   ├── mod.rs           # trait definitions only
│   │   │   ├── users.rs         # UserRepository trait
│   │   │   ├── admin_users.rs   # AdminUserRepository trait
│   │   │   ├── sessions.rs      # SessionRepository trait
│   │   │   ├── prompts.rs       # PromptRepository trait
│   │   │   ├── costs.rs         # CostRepository trait
│   │   │   ├── budgets.rs       # BudgetRepository trait
│   │   │   ├── audit.rs         # AuditRepository trait
│   │   │   └── hooks.rs         # HookRepository trait
│   │   ├── sqlite/
│   │   │   ├── mod.rs           # SqliteDb struct, pool init
│   │   │   ├── users.rs         # UserRepository for SqliteDb
│   │   │   ├── admin_users.rs
│   │   │   ├── sessions.rs
│   │   │   ├── prompts.rs
│   │   │   ├── costs.rs
│   │   │   ├── budgets.rs
│   │   │   ├── audit.rs
│   │   │   └── hooks.rs
│   │   └── postgres/            # feature = "postgres"
│   │       └── (mirrors sqlite/)
│   ├── providers/
│   │   ├── mod.rs
│   │   ├── adapter.rs           # ProviderAdapter trait, NormalizedRequest, CompletionResult
│   │   ├── registry.rs          # cached get_provider(), ProviderSpec
│   │   ├── anthropic.rs         # AnthropicAdapter
│   │   └── openai_compat.rs     # OpenAICompatAdapter
│   ├── router/
│   │   ├── mod.rs
│   │   ├── engine.rs            # RequestRouter, model resolution
│   │   ├── cost.rs              # CostCalculator, pricing table
│   │   ├── policy.rs            # PolicyEngine, budget/rate/allow/deny checks
│   │   └── fallback.rs          # FallbackChain
│   ├── api/
│   │   ├── mod.rs
│   │   ├── app.rs               # axum router factory, AppState
│   │   ├── auth.rs              # bearer token extraction + user lookup middleware
│   │   ├── error.rs             # ApiError → axum IntoResponse
│   │   ├── routes/
│   │   │   ├── mod.rs
│   │   │   ├── completions.rs   # POST /v1/chat/completions
│   │   │   ├── models.rs        # GET /v1/models
│   │   │   └── health.rs        # GET /health
│   │   └── admin/
│   │       ├── mod.rs
│   │       ├── auth.rs          # JWT middleware, login/logout routes
│   │       ├── routes.rs        # /admin/* REST endpoints
│   │       └── dashboard.rs     # /admin/* HTML dashboard routes
│   ├── hooks/
│   │   ├── mod.rs
│   │   ├── lifecycle.rs         # fire-and-forget subprocess hooks
│   │   ├── pipeline.rs          # synchronous stdin/stdout hooks
│   │   └── permissions.rs       # capability check before mutation
│   ├── report/
│   │   ├── mod.rs
│   │   └── formatter.rs         # comfy-table + CSV + JSON output
│   └── cli/
│       ├── mod.rs
│       ├── commands.rs          # clap command definitions
│       └── service.rs           # install-service / uninstall-service
└── tests/
    ├── common/
    │   └── mod.rs               # shared test helpers, in-memory DB setup
    ├── test_config.rs
    ├── test_migrations.rs
    ├── test_router.rs
    ├── test_cost.rs
    ├── test_policy.rs
    ├── test_auth.rs
    ├── test_completions.rs
    ├── test_hooks.rs
    └── test_report.rs
```

---

## Phase 1 — Project Scaffold and Configuration

**Goal:** Compilable project with working config loading, CLI skeleton, and `modelrouter init`.

### Task 1.1 — Initialise Cargo project

```bash
cargo init --name modelrouter
```

Add to `Cargo.toml`:

```toml
[package]
name = "modelrouter"
version = "0.1.0"
edition = "2021"
rust-version = "1.75"

[[bin]]
name = "modelrouter"
path = "src/main.rs"

[lib]
name = "modelrouter"
path = "src/lib.rs"

[features]
default = []
postgres = ["sqlx/postgres"]

[dependencies]
axum = { version = "0.7", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.8", features = ["sqlite", "runtime-tokio", "migrate", "macros"] }
reqwest = { version = "0.12", features = ["json", "stream"] }
clap = { version = "4", features = ["derive"] }
config = "0.14"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
minijinja = { version = "2", features = ["loader"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
thiserror = "1"
anyhow = "1"
bcrypt = "0.15"
jsonwebtoken = "9"
comfy-table = "7"
tower = { version = "0.4", features = ["util"] }
tower-http = { version = "0.5", features = ["trace", "cors"] }
sha2 = "0.10"
hex = "0.4"
time = { version = "0.3", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
axum-test = "14"
tokio = { version = "1", features = ["full"] }
```

> **Note on `[lib]`:** Exposing a lib crate alongside the binary lets integration tests in `tests/` import your types without re-parsing `main.rs`. `main.rs` should be ~10 lines that call into `lib.rs`.

### Task 1.2 — Config schema (`src/config/schema.rs`)

Define structs that map 1:1 to the TOML file. Every field should have a sensible default via `#[serde(default)]`.

```rust
// src/config/schema.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_request_body_limit_mb")]
    pub request_body_limit_mb: usize,
}

fn default_host() -> String { "127.0.0.1".to_string() }
fn default_port() -> u16 { 8080 }
fn default_request_body_limit_mb() -> usize { 10 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: String,                   // "~/.modelrouter/router.db"
    #[serde(default)]
    pub postgres_url: Option<String>,   // set to use postgres feature
}

fn default_db_path() -> String { "~/.modelrouter/router.db".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutingConfig {
    #[serde(default = "default_provider")]
    pub default_provider: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub model_aliases: HashMap<String, String>,
    #[serde(default)]
    pub fallback_chains: HashMap<String, Vec<String>>,
}

fn default_provider() -> String { "openai".to_string() }
fn default_model() -> String { "gpt-4o".to_string() }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    pub api_base: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 { 60 }

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub lifecycle: Vec<LifecycleHookConfig>,
    #[serde(default)]
    pub pipeline: Vec<PipelineHookConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LifecycleHookConfig {
    pub name: String,
    pub event: String,          // "on_request_received" | "on_response_sent" | etc.
    pub exec: String,           // path to executable
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PipelineHookConfig {
    pub name: String,
    pub event: String,          // "pre_request" | "post_response"
    pub exec: String,
    #[serde(default)]
    pub capabilities: Vec<String>,  // ["mutate_request"] — set by operator
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub fail_open: bool,        // true = use original on error; false = return 500
}

fn default_hook_timeout() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthConfig {
    /// Secret for signing admin JWTs. Override via MODELROUTER_AUTH__JWT_SECRET env var.
    #[serde(default = "default_jwt_secret")]
    pub jwt_secret: String,
    /// JWT expiry in minutes.
    #[serde(default = "default_jwt_expiry_mins")]
    pub jwt_expiry_mins: i64,
    /// Overlap window in minutes for token rotation.
    #[serde(default = "default_rotation_overlap_mins")]
    pub rotation_overlap_mins: i64,
}

fn default_jwt_secret() -> String { "change-me-jwt-secret".to_string() }
fn default_jwt_expiry_mins() -> i64 { 60 }
fn default_rotation_overlap_mins() -> i64 { 15 }
```

### Task 1.3 — Config loader (`src/config/mod.rs`)

Load from: `--config` flag → `MODELROUTER_CONFIG` env var → `~/.modelrouter/config.toml`.
Env vars prefixed `MODELROUTER_` override any config file value (use the `config` crate's layering).

```rust
// src/config/mod.rs
pub mod schema;
pub use schema::Settings;

use anyhow::Result;
use config::{Config, Environment, File};
use std::path::PathBuf;

pub fn load(path: Option<PathBuf>) -> Result<Settings> {
    let config_path = path
        .or_else(|| std::env::var("MODELROUTER_CONFIG").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".modelrouter/config.toml")
        });

    let settings = Config::builder()
        .add_source(File::from(config_path).required(false))
        .add_source(Environment::with_prefix("MODELROUTER").separator("__"))
        .build()?
        .try_deserialize::<Settings>()?;

    Ok(settings)
}
```

Add `dirs = "5"` to `Cargo.toml` for the home directory lookup.

### Task 1.4 — Write tests for config (`tests/test_config.rs`)

**Write tests first.** Tests to cover:
- Default values are populated when no config file exists
- A config file is parsed correctly
- Env vars override config file values
- Missing required fields (none — all have defaults) don't panic

```rust
// tests/test_config.rs
#[test]
fn default_settings_parse_without_config_file() {
    let s = modelrouter::config::load(Some("/nonexistent/path.toml".into()))
        .expect("should fall back to defaults");
    assert_eq!(s.server.port, 8080);
    assert_eq!(s.routing.default_model, "gpt-4o");
}

#[test]
fn env_var_overrides_config() {
    std::env::set_var("MODELROUTER_SERVER__PORT", "9090");
    let s = modelrouter::config::load(None).unwrap();
    assert_eq!(s.server.port, 9090);
    std::env::remove_var("MODELROUTER_SERVER__PORT");
}
```

### Task 1.5 — CLI skeleton (`src/cli/commands.rs`)

Use clap derive. Define subcommands now; bodies can be `todo!()` stubs until implemented in later phases.

```rust
// src/cli/commands.rs
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "modelrouter", version, about = "Self-hosted LLM proxy with budget controls")]
pub struct Cli {
    #[arg(long, global = true, env = "MODELROUTER_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialise config file and database
    Init,
    /// Start the proxy server
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
    /// Run database migrations
    Migrate,
    /// Manage proxy users
    User(UserArgs),
    /// Manage budget rules
    Budget(BudgetArgs),
    /// Generate reports
    Report(ReportArgs),
    /// View audit log
    Audit {
        #[arg(long, default_value_t = 50)]
        tail: u32,
    },
    /// Install or remove the system service
    InstallService,
    UninstallService,
}

// ... UserArgs, BudgetArgs, ReportArgs with their own subcommands
```

### Task 1.6 — `modelrouter init` command

Creates `~/.modelrouter/config.toml` from an embedded annotated template if it does not already exist. Prints the path on success.

Embed the template with `include_str!("../../config.example.toml")` in the init handler.

### Task 1.7 — `src/main.rs`

```rust
// src/main.rs
use clap::Parser;
use modelrouter::cli::commands::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    modelrouter::cli::run(cli).await
}
```

`src/lib.rs` re-exports all public modules and contains `pub async fn cli::run(cli: Cli)`.

### Task 1.8 — `config.example.toml`

Write a fully annotated example. Every field should have a comment explaining what it does and what the valid values are. This is the primary user-facing documentation for configuration.

```toml
# modelrouter configuration
# Copy to ~/.modelrouter/config.toml and edit.
# All values can be overridden with env vars: MODELROUTER_<SECTION>__<KEY>
# e.g. MODELROUTER_SERVER__PORT=9090

[server]
host = "127.0.0.1"  # bind address; use "0.0.0.0" to accept external connections
port = 8080
request_body_limit_mb = 10  # max request body size; protects against payload floods

[database]
path = "~/.modelrouter/router.db"  # tilde-expanded; will be created if absent
# postgres_url = "postgres://user:pass@localhost/modelrouter"  # uncomment for postgres

[routing]
default_provider = "anthropic"
default_model = "claude-haiku-4-5"  # used when client omits the model field

[routing.model_aliases]
fast = "anthropic/claude-haiku-4-5"
smart = "anthropic/claude-opus-4-6"
cheap = "openai/gpt-4o-mini"

[providers.anthropic]
api_key = ""  # or set MODELROUTER_PROVIDERS__ANTHROPIC__API_KEY

[providers.openai]
api_key = ""

# Gemini via its OpenAI-compatible endpoint
[providers.gemini]
api_key = ""
api_base = "https://generativelanguage.googleapis.com/v1beta/openai/"

# Local Ollama — no key needed
[providers.ollama]
api_key = "not-required"
api_base = "http://localhost:11434/v1"

[auth]
jwt_secret = "change-me"  # MUST change in production; use MODELROUTER_AUTH__JWT_SECRET
jwt_expiry_mins = 60
rotation_overlap_mins = 15  # how long old key stays valid after rotation

# Lifecycle hook — fire-and-forget, cannot mutate request/response
[[hooks.lifecycle]]
name = "slack-budget-alert"
event = "on_budget_exceeded"
exec = "/etc/modelrouter/hooks/slack-alert.sh"
timeout_secs = 5

# Pipeline hook — synchronous, CAN mutate if granted capability
[[hooks.pipeline]]
name = "inject-system-prompt"
event = "pre_request"
exec = "/etc/modelrouter/hooks/inject-prompt.sh"
capabilities = ["mutate_request"]  # operator-granted; hook cannot self-grant
timeout_secs = 2
fail_open = true  # on timeout/error, use original request (don't return 500)
```

### Commit after Phase 1
```
git add -A && git commit -m "feat: Phase 1 — project scaffold, config, CLI skeleton"
```

---

## Phase 2 — Database Layer

**Goal:** Schema created, all Repository traits defined, SQLite implementations working, migrations idempotent. No HTTP yet.

### Task 2.1 — Migration SQL (`migrations/001_initial.sql`)

This is the single source of truth for the schema. `sqlx migrate` applies it.

```sql
-- migrations/001_initial.sql
CREATE TABLE IF NOT EXISTS users (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    api_key     TEXT NOT NULL UNIQUE,       -- SHA-256 hex of bearer token
    api_key_old TEXT,                       -- previous key during rotation window
    api_key_old_expires_at TEXT,            -- ISO-8601; NULL means no rotation in progress
    group_name  TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS admin_users (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    password_hash   TEXT NOT NULL,          -- bcrypt
    role            TEXT NOT NULL DEFAULT 'viewer', -- 'superadmin' | 'viewer'
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL,
    last_login_at   TEXT
);

CREATE TABLE IF NOT EXISTS budget_rules (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER REFERENCES users(id) ON DELETE CASCADE,
    group_name   TEXT,
    window       TEXT NOT NULL,             -- 'daily' | 'weekly' | 'monthly'
    limit_usd    REAL,
    limit_tokens INTEGER,
    model_allow  TEXT NOT NULL DEFAULT '[]',  -- JSON array of model names
    model_deny   TEXT NOT NULL DEFAULT '[]',
    rate_rpm     INTEGER,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    external_id TEXT,
    project     TEXT,
    created_at  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS prompts (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id           INTEGER NOT NULL REFERENCES users(id),
    session_id        INTEGER REFERENCES sessions(id),
    request_model     TEXT NOT NULL,
    routed_model      TEXT NOT NULL,
    provider          TEXT NOT NULL,
    messages          TEXT NOT NULL,        -- JSON array
    response          TEXT,
    finish_reason     TEXT,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL NOT NULL DEFAULT 0.0,
    latency_ms        INTEGER,
    tags              TEXT NOT NULL DEFAULT '[]',
    project           TEXT,
    created_at        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS cost_ledger (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    prompt_id  INTEGER NOT NULL REFERENCES prompts(id),
    model      TEXT NOT NULL,
    provider   TEXT NOT NULL,
    project    TEXT,
    tokens_in  INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    cost_usd   REAL NOT NULL DEFAULT 0.0,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rate_limit_state (
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    window_key    TEXT NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, window_key)
);

CREATE TABLE IF NOT EXISTS hook_permissions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    hook_name  TEXT NOT NULL,
    capability TEXT NOT NULL,               -- 'mutate_request' | 'mutate_response'
    granted_by INTEGER REFERENCES admin_users(id),
    granted_at TEXT NOT NULL,
    UNIQUE(hook_name, capability)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id   INTEGER REFERENCES admin_users(id),
    actor_name TEXT NOT NULL,               -- denormalised for tombstoned admins
    action     TEXT NOT NULL,               -- 'create_user' | 'set_budget' | etc.
    target     TEXT,                        -- e.g. "user:alice"
    before_json TEXT,                       -- state before change
    after_json  TEXT,                       -- state after change
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS hook_metrics (
    hook_name   TEXT NOT NULL,
    invoked_at  TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    success     INTEGER NOT NULL DEFAULT 1
);

-- Indices for common query patterns
CREATE INDEX IF NOT EXISTS idx_prompts_user_created ON prompts(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_user_created ON cost_ledger(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_audit_log_actor ON audit_log(actor_id, created_at);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
```

### Task 2.2 — Repository traits (`src/db/repositories/`)

Each trait is pure Rust, no SQLx in the trait itself. This keeps the trait implementable by a mock.

```rust
// src/db/repositories/users.rs
use async_trait::async_trait;
use crate::db::models::{User, NewUser};

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn find_by_api_key(&self, key_hash: &str) -> anyhow::Result<Option<User>>;
    async fn find_by_name(&self, name: &str) -> anyhow::Result<Option<User>>;
    async fn list(&self) -> anyhow::Result<Vec<User>>;
    async fn create(&self, user: NewUser) -> anyhow::Result<User>;
    async fn set_enabled(&self, id: i64, enabled: bool) -> anyhow::Result<()>;
    async fn rotate_key(
        &self,
        id: i64,
        new_key_hash: &str,
        overlap_expires_at: &str,
    ) -> anyhow::Result<()>;
    async fn expire_old_keys(&self) -> anyhow::Result<u64>;  // called periodically
}
```

Define similar traits for `AdminUserRepository`, `SessionRepository`, `PromptRepository`, `CostRepository`, `BudgetRepository`, `AuditRepository`, `HookRepository`.

Add `async-trait = "0.1"` to `Cargo.toml` until async trait stabilises.

### Task 2.3 — Domain models (`src/db/models.rs`)

Plain structs with `serde::Deserialize` (for sqlx `FromRow`) and `serde::Serialize` (for JSON responses).

```rust
// src/db/models.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub api_key: String,
    pub api_key_old: Option<String>,
    pub api_key_old_expires_at: Option<String>,
    pub group_name: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub metadata: String,  // raw JSON
}

#[derive(Debug)]
pub struct NewUser {
    pub name: String,
    pub api_key_hash: String,
    pub group_name: Option<String>,
}
```

### Task 2.4 — SQLite implementation (`src/db/sqlite/`)

```rust
// src/db/sqlite/mod.rs
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::str::FromStr;

#[derive(Clone)]
pub struct SqliteDb {
    pub pool: SqlitePool,
}

impl SqliteDb {
    pub async fn connect(path: &str) -> anyhow::Result<Self> {
        let expanded = shellexpand::tilde(path).into_owned();
        // create parent directory if needed
        if let Some(parent) = std::path::Path::new(&expanded).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", expanded))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePool::connect_with(opts).await?;
        Ok(Self { pool })
    }
}
```

Add `shellexpand = "3"` to `Cargo.toml`.

Implement each repository trait for `SqliteDb`. Use `sqlx::query_as!` macro for compile-time checking. Example:

```rust
// src/db/sqlite/users.rs
use async_trait::async_trait;
use crate::db::{models::{User, NewUser}, repositories::users::UserRepository};
use super::SqliteDb;

#[async_trait]
impl UserRepository for SqliteDb {
    async fn find_by_api_key(&self, key_hash: &str) -> anyhow::Result<Option<User>> {
        let user = sqlx::query_as!(
            User,
            r#"SELECT id, name, api_key, api_key_old, api_key_old_expires_at,
                      group_name, enabled as "enabled: bool", created_at, metadata
               FROM users
               WHERE api_key = ? OR (api_key_old = ? AND api_key_old_expires_at > datetime('now'))
               LIMIT 1"#,
            key_hash, key_hash
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(user)
    }
    // ... other methods
}
```

> **Key lesson from Python version:** `find_by_api_key` must check BOTH `api_key` AND `api_key_old` (within the expiry window) to support token rotation without downtime.

### Task 2.5 — Migrations runner (`src/db/migrations.rs`)

```rust
pub async fn run_migrations(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    warn_if_dev_key_active(pool).await?;
    Ok(())
}

async fn warn_if_dev_key_active(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    let dev_hash = hash_token("mr-dev-key");
    let row = sqlx::query!("SELECT id FROM users WHERE api_key = ?", dev_hash)
        .fetch_optional(pool)
        .await?;
    if row.is_some() {
        tracing::warn!(
            "SECURITY: default dev API key (mr-dev-key) is still active. \
             Rotate or disable before production use."
        );
    }
    Ok(())
}
```

### Task 2.6 — Dev seed

In `migrations/`, add a separate seed file that is **only run when the `MODELROUTER_DEV_SEED=true` env var is set**. Never auto-seed in production. The `modelrouter init` command runs the seed when scaffolding a dev environment.

### Task 2.7 — Tests (`tests/test_migrations.rs`)

```rust
// tests/common/mod.rs
pub async fn in_memory_db() -> SqliteDb {
    let db = SqliteDb::connect(":memory:").await.unwrap();
    run_migrations(&db.pool).await.unwrap();
    db
}
```

Tests to write:
- `migrations_create_all_tables()` — query `sqlite_master`, assert all 10 tables exist
- `migrations_are_idempotent()` — run migrations twice, assert no error, version stays at latest
- `create_and_find_user()` — insert via `UserRepository`, fetch by key hash
- `token_rotation_overlap_window()` — insert user, rotate key, assert old key works until expiry
- `old_key_rejected_after_expiry()` — set `api_key_old_expires_at` in the past, assert old key rejected

### Commit after Phase 2
```
git commit -m "feat: Phase 2 — database layer, migrations, repository traits + SQLite impl"
```

---

## Phase 3 — Core Proxy (MVP)

**Goal:** A working proxy. `modelrouter serve` accepts requests, authenticates them, routes to Anthropic or OpenAI, streams the response, logs cost. No budget enforcement yet.

### Task 3.1 — Provider types (`src/providers/adapter.rs`)

```rust
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use futures::Stream;
use bytes::Bytes;

#[derive(Debug, Clone)]
pub struct NormalizedRequest {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    pub stream: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub extra_params: serde_json::Value,  // passthrough for unknown keys
}

#[derive(Debug, Clone)]
pub struct CompletionResult {
    pub content: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub finish_reason: String,
}

pub type SseStream = Pin<Box<dyn Stream<Item = anyhow::Result<Bytes>> + Send>>;

#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult>;
    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream>;
}
```

Add `futures = "0.3"` and `bytes = "1"` to `Cargo.toml`.

### Task 3.2 — OpenAI-compat adapter (`src/providers/openai_compat.rs`)

Use `reqwest` directly — no OpenAI SDK. The OpenAI chat completions API is simple JSON over HTTP; you do not need a crate for it.

Key points:
- Non-streaming: `POST /chat/completions`, deserialise response, return `CompletionResult`
- Streaming: same endpoint with `"stream": true`, return SSE byte stream via `response.bytes_stream()`
- `api_base` defaults to `https://api.openai.com/v1`
- Auth header: `Authorization: Bearer <api_key>`

```rust
pub struct OpenAICompatAdapter {
    client: reqwest::Client,
    api_base: String,
    api_key: String,
}

impl OpenAICompatAdapter {
    pub fn new(config: &ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_base: config.api_base.clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            api_key: config.api_key.clone(),
        }
    }
}
```

### Task 3.3 — Anthropic adapter (`src/providers/anthropic.rs`)

The Anthropic Messages API differs from OpenAI in three ways:
1. Auth header is `x-api-key: <key>` not `Authorization: Bearer <key>`
2. System messages go in a separate top-level `system` field, not in the messages array
3. The messages array only accepts `user` and `assistant` roles

Write a `translate_messages(messages: &[Value]) -> (Option<String>, Vec<Value>)` function that extracts system messages (concatenating if multiple) and filters the remaining array. This is the most logic-heavy part of this adapter — test it thoroughly with edge cases (no system message, multiple system messages, unknown roles).

### Task 3.4 — Provider registry (`src/providers/registry.rs`)

Cache adapter instances. Use a `DashMap<(String, String, Option<String>), Arc<dyn ProviderAdapter>>` keyed by `(provider_name, api_key, api_base)`. Lesson from Python: creating a new reqwest `Client` per request discards connection pools.

Add `dashmap = "6"` to `Cargo.toml`.

### Task 3.5 — Cost calculator (`src/router/cost.rs`)

```rust
pub struct CostCalculator {
    pricing: HashMap<&'static str, ModelPricing>,
}

struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
}

impl CostCalculator {
    pub fn new() -> Self { /* hard-coded pricing table */ }

    pub fn calculate(&self, model: &str, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        // strip provider prefix, lowercase, look up, return 0.0 for unknown
    }
}
```

Pricing table must match the Python version. Unknown models (Ollama) return `0.0` — intentional.

### Task 3.6 — Request router (`src/router/engine.rs`)

```rust
pub struct RequestRouter {
    settings: Arc<Settings>,
}

impl RequestRouter {
    pub fn resolve(&self, requested_model: &str) -> (String, String) {
        // returns (provider_name, canonical_model)
        // 1. alias lookup in routing.model_aliases
        // 2. split on "/" for explicit provider prefix
        // 3. fall back to routing.default_provider + routing.default_model
    }
}
```

### Task 3.7 — API key auth (`src/api/auth.rs`)

Implement as an axum extractor. The extractor hashes the bearer token with SHA-256, looks up in the `users` table (both current and rotation-overlap key), checks `enabled`. Returns `AuthenticatedUser` on success or an `ApiError::Unauthorized` that axum turns into a 401.

```rust
pub fn hash_token(token: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}
```

### Task 3.8 — App state and axum app (`src/api/app.rs`)

```rust
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: Arc<dyn DatabaseProvider>,  // trait object wrapping the concrete DB
    pub router: Arc<RequestRouter>,
    pub cost_calc: Arc<CostCalculator>,
    pub provider_registry: Arc<ProviderRegistry>,
}
```

`DatabaseProvider` is a single trait that aggregates all repository traits:
```rust
pub trait DatabaseProvider:
    UserRepository + AdminUserRepository + SessionRepository +
    PromptRepository + CostRepository + BudgetRepository +
    AuditRepository + HookRepository +
    Send + Sync {}
```

`SqliteDb` implements `DatabaseProvider`. This is the single thing you pass into `AppState`, keeping the app code clean.

### Task 3.9 — Completions endpoint (`src/api/routes/completions.rs`)

This is the hot path. Keep it lean.

```rust
pub async fn chat_completions(
    State(state): State<AppState>,
    user: AuthenticatedUser,   // extractor — 401 if missing/invalid
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    let model = body["model"].as_str().unwrap_or(&state.settings.routing.default_model);
    let stream = body["stream"].as_bool().unwrap_or(false);

    // Run pre-request pipeline hooks (Phase 5 — stub here)

    let (provider_name, canonical_model) = state.router.resolve(model);
    let norm_req = build_normalized_request(&body, canonical_model);
    let adapter = state.provider_registry.get(&provider_name, ...)?;

    let request_id = format!("chatcmpl-mr-{}", uuid::Uuid::new_v4());
    let start = std::time::Instant::now();

    if stream {
        let sse_stream = adapter.stream(&norm_req).await
            .map_err(ApiError::ProviderError)?;
        // wrap in StreamBody for axum, record cost async after stream ends
        // Post-response pipeline hooks go here (Phase 5 — stub)
        return Ok(streaming_response(sse_stream, request_id));
    }

    let result = adapter.complete(&norm_req).await
        .map_err(ApiError::ProviderError)?;
    let latency_ms = start.elapsed().as_millis() as i64;
    let cost = state.cost_calc.calculate(&canonical_model, result.prompt_tokens, result.completion_tokens);

    // Fire-and-forget: log prompt + cost
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = record_prompt(&state_clone, ...).await {
            tracing::error!("Failed to record prompt: {}", e);
        }
    });

    // Post-response pipeline hooks (Phase 5 — stub)
    Ok(Json(build_openai_response(request_id, result)).into_response())
}
```

**SSE streaming fix (lesson from Python):** When accumulating streaming content for token estimation, extract only the `choices[0].delta.content` text from each SSE chunk, not the full SSE string. Write `fn extract_text_from_sse(chunk: &[u8]) -> Option<String>` and test it explicitly.

### Task 3.10 — Error type (`src/api/error.rs`)

```rust
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("provider error: {0}")]
    ProviderError(#[from] anyhow::Error),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("internal error")]
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        // map to appropriate status code + OpenAI-shaped error JSON
    }
}
```

### Task 3.11 — Tests (`tests/test_completions.rs`, `tests/test_router.rs`, `tests/test_cost.rs`)

Use `axum-test` for HTTP-level integration tests. Mock the provider adapter by implementing `ProviderAdapter` for a `MockAdapter` struct in `tests/common/`.

Tests to write:
- `resolve_explicit_provider_prefix()` — `"openai/gpt-4o"` → `("openai", "gpt-4o")`
- `resolve_alias()` — `"fast"` → resolves via alias map
- `resolve_default()` — no prefix, no alias → `(default_provider, default_model)`
- `missing_model_uses_default_model()` — omitting model in body uses `routing.default_model`
- `unauthenticated_request_returns_401()`
- `disabled_user_returns_401()`
- `valid_request_returns_200()`
- `streaming_request_returns_sse()`
- `error_response_includes_request_id()`
- `cost_calculation_gpt4o()`
- `cost_calculation_unknown_model_returns_zero()`
- `extract_text_from_sse_chunk_returns_delta_content()`
- `extract_text_from_done_returns_empty()`

### Commit after Phase 3
```
git commit -m "feat: Phase 3 — core proxy MVP, provider adapters, auth, cost logging"
```

---

## Phase 4 — Budget Controls, Policy, and Admin Auth

**Goal:** Per-user budget limits enforced before forwarding. Named admin accounts with JWT sessions. Audit log. Token rotation.

### Task 4.1 — Policy engine (`src/router/policy.rs`)

```rust
pub struct PolicyEngine {
    db: Arc<dyn DatabaseProvider>,
}

pub enum PolicyDecision {
    Allow,
    Deny { reason: String, status: u16 },
}

impl PolicyEngine {
    /// Called before forwarding to provider.
    pub async fn check(&self, user: &User, model: &str) -> anyhow::Result<PolicyDecision> {
        // 1. Check model_allow (if set, model must be in list)
        // 2. Check model_deny (if model in list, reject)
        // 3. Check rate limit (rate_limit_state table; window_key = "rpm:{minute_bucket}")
        // 4. Check budget (sum cost_ledger WHERE user_id=? AND created_at > window_start)
        // Return first Deny found, or Allow
    }
}
```

**Budget check query:**
```sql
SELECT COALESCE(SUM(cost_usd), 0) as total
FROM cost_ledger
WHERE user_id = ?
  AND created_at >= ?   -- window start (daily/weekly/monthly)
```

This is O(window rows), not O(all prompts) — the denormalised `cost_ledger` table exists for exactly this reason.

### Task 4.2 — Wire policy into completions endpoint

After auth, before provider dispatch:
```rust
match state.policy.check(&user, &model).await? {
    PolicyDecision::Allow => {}
    PolicyDecision::Deny { reason, status } => {
        // fire on_budget_exceeded lifecycle hook (Phase 5 stub)
        return Err(ApiError::PolicyDenied { reason, status });
    }
}
```

### Task 4.3 — Admin user model and bcrypt auth

Admin login: `POST /admin/login` accepts `{"name": "...", "password": "..."}`, verifies bcrypt hash, issues JWT. The JWT payload contains `admin_id`, `role`, `exp`.

```rust
#[derive(Serialize, Deserialize)]
struct AdminClaims {
    sub: i64,       // admin_users.id
    name: String,
    role: String,   // "superadmin" | "viewer"
    exp: usize,     // unix timestamp
}
```

### Task 4.4 — Admin JWT middleware

Axum extractor `AdminSession` that reads the JWT from the `Authorization: Bearer` header (API calls) or an `HttpOnly` cookie (dashboard). Rejects with 401 if missing, expired, or invalid. Attaches `AdminClaims` to the request.

### Task 4.5 — Audit log middleware

Write a helper `audit(db, actor, action, target, before, after)` that inserts a row into `audit_log`. Call it in every admin route handler that mutates state. This is not middleware — it is an explicit call at the end of each mutation handler so the log is accurate.

### Task 4.6 — Admin REST API (`src/api/admin/routes.rs`)

All routes require `AdminSession` extractor.

| Method | Path | Auth | Description |
|---|---|---|---|
| GET | /admin/api/users | any admin | List users |
| POST | /admin/api/users | superadmin | Create user |
| PATCH | /admin/api/users/:id | superadmin | Enable/disable |
| POST | /admin/api/users/:id/rotate-key | superadmin | Rotate API key |
| GET | /admin/api/budgets | any admin | List budget rules |
| POST | /admin/api/budgets | superadmin | Create/update budget |
| DELETE | /admin/api/budgets/:id | superadmin | Delete budget |
| GET | /admin/api/stats | any admin | Aggregate cost stats |
| GET | /admin/api/audit | any admin | Audit log |
| GET | /admin/api/prompts | any admin | Prompt list |
| GET | /admin/api/admins | superadmin | List admin accounts |
| POST | /admin/api/admins | superadmin | Create admin account |

### Task 4.7 — Token rotation (`src/db/sqlite/users.rs`)

`rotate_key()` implementation:
1. Set `api_key_old = api_key`, `api_key_old_expires_at = now + overlap_mins`
2. Set `api_key = new_hash`
3. `find_by_api_key` checks BOTH columns (already covered in Task 2.4)
4. A periodic task (or on-auth check) calls `expire_old_keys()` which NULLs expired `api_key_old` rows

### Task 4.8 — CLI user and budget commands

```bash
modelrouter user create --name alice [--group engineering]
modelrouter user list
modelrouter user disable alice
modelrouter user rotate-key alice
modelrouter budget set --user alice --window monthly --limit-usd 50.00
modelrouter budget list [--user alice]
modelrouter budget delete <id>
```

### Task 4.9 — Tests (`tests/test_policy.rs`, `tests/test_auth.rs`)

Tests to write:
- `budget_exceeded_returns_429()`
- `model_in_deny_list_returns_403()`
- `model_not_in_allow_list_returns_403()`
- `rate_limit_exceeded_returns_429()`
- `under_budget_allows_request()`
- `admin_login_valid_credentials_returns_jwt()`
- `admin_login_wrong_password_returns_401()`
- `admin_viewer_cannot_create_user_returns_403()`
- `token_rotation_old_key_works_within_window()`
- `token_rotation_old_key_rejected_after_window()`
- `audit_log_written_on_user_creation()`

### Commit after Phase 4
```
git commit -m "feat: Phase 4 — policy engine, budget enforcement, admin auth, token rotation"
```

---

## Phase 5 — Hook System

**Goal:** Lifecycle and pipeline hooks wired into the request flow, with operator-controlled permissions.

### Task 5.1 — Lifecycle hook runner (`src/hooks/lifecycle.rs`)

```rust
pub async fn fire(hook: &LifecycleHookConfig, payload: serde_json::Value) {
    // tokio::spawn so caller is never blocked
    let hook = hook.clone();
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            Duration::from_secs(hook.timeout_secs),
            run_subprocess(&hook.exec, &payload),
        ).await;
        match result {
            Err(_timeout) => tracing::warn!(hook = %hook.name, "lifecycle hook timed out"),
            Ok(Err(e)) => tracing::error!(hook = %hook.name, "lifecycle hook error: {}", e),
            Ok(Ok(exit)) if !exit.success() => {
                tracing::warn!(hook = %hook.name, "lifecycle hook exited non-zero")
            }
            Ok(Ok(_)) => {}
        }
    });
}

async fn run_subprocess(exec: &str, payload: &serde_json::Value) -> anyhow::Result<std::process::ExitStatus> {
    use tokio::process::Command;
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new(exec)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(serde_json::to_string(payload)?.as_bytes()).await?;
    }
    Ok(child.wait().await?)
}
```

**Lifecycle events and their payloads:**

| Event | Payload fields |
|---|---|
| `on_request_received` | `user_name`, `model`, `message_count`, `timestamp` |
| `on_response_sent` | `user_name`, `model`, `routed_model`, `cost_usd`, `latency_ms` |
| `on_budget_exceeded` | `user_name`, `model`, `limit_usd`, `spent_usd`, `window` |
| `on_stream_complete` | `user_name`, `model`, `approx_tokens`, `cost_usd` |
| `on_error` | `user_name`, `model`, `error_type`, `message` |
| `on_user_disabled` | `user_name`, `disabled_by` |

### Task 5.2 — Pipeline hook runner (`src/hooks/pipeline.rs`)

```rust
pub async fn run_pipeline_hook(
    hook: &PipelineHookConfig,
    payload: serde_json::Value,
    db: &Arc<dyn DatabaseProvider>,
) -> anyhow::Result<serde_json::Value> {
    // 1. Check permission (if hook claims mutate_request, verify in hook_permissions table)
    let can_mutate = check_permission(db, &hook.name, required_capability(hook)).await?;

    let result = tokio::time::timeout(
        Duration::from_secs(hook.timeout_secs),
        run_subprocess_bidirectional(&hook.exec, &payload),
    ).await;

    match result {
        Ok(Ok(output)) if can_mutate => {
            // parse output as JSON; if invalid, apply fail_open logic
            serde_json::from_str(&output)
                .map_err(|e| handle_parse_error(hook, e))
        }
        Ok(Ok(_)) => Ok(payload),  // hook ran but has no mutate permission → discard output
        Err(_timeout) | Ok(Err(_)) => {
            if hook.fail_open { Ok(payload) } else { Err(ApiError::HookFailed.into()) }
        }
    }
}
```

`run_subprocess_bidirectional` writes payload JSON to stdin and reads stdout as the (potentially modified) payload.

### Task 5.3 — Permissions check (`src/hooks/permissions.rs`)

```rust
pub async fn check_permission(
    db: &Arc<dyn DatabaseProvider>,
    hook_name: &str,
    capability: &str,
) -> anyhow::Result<bool> {
    db.hook_has_permission(hook_name, capability).await
}
```

On startup, sync `config.toml` hook capabilities into `hook_permissions` table. The table is the runtime source of truth; config is the declaration. Admin can revoke a capability via API without restarting the server.

### Task 5.4 — Wire hooks into request flow

In `completions.rs`:

```rust
// After auth, before policy check
fire_lifecycle(&state, "on_request_received", request_payload(&user, &model));

// After policy check, before provider — pre_request pipeline
let body = run_pre_request_hooks(&state, body).await?;

// ... provider call ...

// After provider responds — post_response pipeline
let result = run_post_response_hooks(&state, result).await?;

// After response sent
fire_lifecycle(&state, "on_response_sent", response_payload(&user, &result));
```

### Task 5.5 — Hook metrics

After every hook execution, insert a row into `hook_metrics`:
```sql
INSERT INTO hook_metrics (hook_name, invoked_at, duration_ms, success) VALUES (?, ?, ?, ?)
```

The dashboard and `modelrouter report hooks` query this table for p50/p95/p99.

### Task 5.6 — Tests (`tests/test_hooks.rs`)

Tests to write:
- `lifecycle_hook_fires_without_blocking_response()`
- `lifecycle_hook_timeout_does_not_affect_response()`
- `pipeline_hook_mutates_request_when_permitted()`
- `pipeline_hook_cannot_mutate_without_permission()`
- `pipeline_hook_timeout_with_fail_open_returns_original()`
- `pipeline_hook_timeout_with_fail_closed_returns_500()`
- `hook_metrics_recorded_after_execution()`

### Commit after Phase 5
```
git commit -m "feat: Phase 5 — lifecycle and pipeline hook system with permission controls"
```

---

## Phase 6 — Reporting CLI

**Goal:** All `modelrouter report` subcommands work, producing human-readable tables and machine-readable CSV/JSON.

### Task 6.1 — Report query layer (`src/report/mod.rs`)

Write dedicated query functions (not repository methods — these are analytics, not CRUD):

```rust
pub async fn cost_by_user_window(
    db: &Arc<dyn DatabaseProvider>,
    window: Window,
    user_name: Option<&str>,
) -> anyhow::Result<Vec<CostRow>>;

pub async fn usage_by_model(
    db: &Arc<dyn DatabaseProvider>,
    since: Option<chrono::DateTime<Utc>>,
) -> anyhow::Result<Vec<UsageRow>>;

pub async fn recent_prompts(
    db: &Arc<dyn DatabaseProvider>,
    user_name: Option<&str>,
    limit: u32,
) -> anyhow::Result<Vec<PromptRow>>;

pub async fn hook_latency_stats(
    db: &Arc<dyn DatabaseProvider>,
) -> anyhow::Result<Vec<HookStats>>;

// p50/p95/p99 computed in SQL using percentile approximation
```

Add `chrono = { version = "0.4", features = ["serde"] }` to `Cargo.toml`.

### Task 6.2 — Formatter (`src/report/formatter.rs`)

```rust
pub enum OutputFormat { Table, Csv, Json }

pub fn print_table(headers: &[&str], rows: &[Vec<String>], format: OutputFormat) {
    match format {
        OutputFormat::Table => { /* comfy-table */ }
        OutputFormat::Csv => { /* write CSV to stdout */ }
        OutputFormat::Json => { /* serde_json::to_writer(stdout) */ }
    }
}
```

### Task 6.3 — CLI commands

```bash
modelrouter report cost [--user alice] [--window daily|weekly|monthly] [--format table|csv|json]
modelrouter report usage [--model gpt-4o] [--project myproject] [--since 2026-01-01]
modelrouter report prompts [--user alice] [--limit 50] [--since 2026-01-01]
modelrouter report audit [--actor alice] [--tail 50]
modelrouter report hooks
```

### Task 6.4 — Tests (`tests/test_report.rs`)

- `cost_report_sums_cost_ledger_by_window()`
- `cost_report_filters_by_user()`
- `usage_report_groups_by_model()`
- `json_format_is_valid_parseable_json()`
- `csv_format_has_correct_headers()`

### Commit after Phase 6
```
git commit -m "feat: Phase 6 — reporting CLI (cost, usage, prompts, audit, hooks)"
```

---

## Phase 7 — Admin Dashboard

**Goal:** Web dashboard at `/admin` with all key views, HTMX-powered, served from the same binary.

### Task 7.1 — Template setup

Embed templates at compile time using `minijinja`'s `Environment` with sources loaded via `include_str!`. Templates live in `templates/admin/`. The base layout (`base.html`) includes the HTMX CDN script tag and defines blocks for `title`, `content`, and `scripts`.

```rust
// src/api/admin/dashboard.rs
use minijinja::{Environment, context};

pub fn build_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_template("base.html", include_str!("../../../templates/admin/base.html")).unwrap();
    env.add_template("overview.html", include_str!("../../../templates/admin/overview.html")).unwrap();
    // ... etc
    env
}
```

### Task 7.2 — Dashboard middleware

Axum middleware that checks for a valid JWT cookie. On missing/expired JWT, redirects to `/admin/login`. Attach `AdminClaims` to request extensions.

### Task 7.3 — Login / logout

`GET /admin/login` — render login form
`POST /admin/login` — verify password, issue JWT as `HttpOnly; SameSite=Strict` cookie, redirect to `/admin`
`POST /admin/logout` — clear cookie, redirect to login

### Task 7.4 — Dashboard routes

| Route | Template | HTMX targets |
|---|---|---|
| GET /admin | overview.html | spend summary, budget alerts |
| GET /admin/users | users.html | user table, create form, disable/rotate buttons |
| GET /admin/prompts | prompts.html | paginated table, expand row on click |
| GET /admin/cost | cost.html | cost breakdown table |
| GET /admin/hooks | hooks.html | hook list + latency table |
| GET /admin/audit | audit.html | audit log, filterable |
| GET /admin/admins | admins.html | admin account management (superadmin only) |

HTMX pattern: each action (disable user, rotate key, set budget) posts to an `/admin/api/*` REST endpoint, which returns an HTML fragment that replaces the relevant table row. No page reload needed. No JavaScript state.

### Task 7.5 — Tests

- `unauthenticated_dashboard_redirects_to_login()`
- `login_with_valid_credentials_sets_cookie()`
- `overview_page_renders_without_error()`
- `viewer_cannot_see_admin_management_page()`

### Commit after Phase 7
```
git commit -m "feat: Phase 7 — admin dashboard (HTMX + minijinja, all views)"
```

---

## Phase 8 — Deployment and Postgres

**Goal:** Multi-arch binary releases, Docker, service install commands, Postgres support.

### Task 8.1 — GitHub Actions (`.github/workflows/release.yml`)

Trigger on version tags (`v*`). Build matrix:

```yaml
strategy:
  matrix:
    include:
      - target: x86_64-unknown-linux-musl
        os: ubuntu-latest
      - target: aarch64-unknown-linux-musl
        os: ubuntu-latest
      - target: x86_64-apple-darwin
        os: macos-latest
      - target: aarch64-apple-darwin
        os: macos-latest
```

Use `cross` for Linux musl targets (static binaries). Sign macOS binaries with `codesign` if a certificate is available.

Upload binaries as GitHub Release assets. Name them `modelrouter-{target}`.

### Task 8.2 — Dockerfile

```dockerfile
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY modelrouter /usr/local/bin/modelrouter
ENTRYPOINT ["modelrouter"]
CMD ["serve"]
```

Multi-stage: builder stage uses `rust:1.75` + `cargo build --release`, runtime stage is slim. The SQLite bundled feature means no `libsqlite3` dependency in the runtime image.

### Task 8.3 — Service install commands (`src/cli/service.rs`)

`modelrouter install-service`:
- macOS: write `contrib/dev.modelrouter.plist` (embedded via `include_str!`) to `~/Library/LaunchAgents/`, run `launchctl load`
- Linux: write `contrib/modelrouter.service` (embedded) to `/etc/systemd/system/`, run `systemctl daemon-reload && systemctl enable modelrouter`

Both detect the current binary path and write it into the service file. `modelrouter uninstall-service` reverses the above.

### Task 8.4 — Postgres support (`src/db/postgres/`)

Mirror the `src/db/sqlite/` module. Use `sqlx::query_as!` with Postgres-compatible syntax (positional `$1` instead of `?`). Gate the entire module behind `#[cfg(feature = "postgres")]`.

Config: if `database.postgres_url` is set, use the Postgres pool; otherwise, use SQLite. This is the only branch in `main.rs`.

### Task 8.5 — `modelrouter init` polish

The first-run experience:
1. Create `~/.modelrouter/` directory
2. Write annotated `config.toml` from embedded template
3. Run migrations
4. If `MODELROUTER_DEV_SEED=true`, create dev user and print `mr-dev-key`
5. Print:
   ```
   modelrouter initialised.
   Config: ~/.modelrouter/config.toml
   Database: ~/.modelrouter/router.db

   Next: edit config.toml to add provider API keys, then run:
     modelrouter serve
   ```

### Task 8.6 — Tag v0.1.0

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

### Commit after Phase 8
```
git commit -m "feat: Phase 8 — deployment (multi-arch, Docker, service install, Postgres)"
```

---

## Testing Reference

### Running tests

```bash
# All tests
cargo test

# Specific module
cargo test test_policy

# With output (useful for debugging)
cargo test -- --nocapture

# Integration tests only
cargo test --test '*'

# With Postgres feature
cargo test --features postgres
```

### Test database

Every integration test that needs a DB calls `common::in_memory_db()` which opens a `:memory:` SQLite connection and runs migrations. Tests are fully isolated — no shared state between tests.

### Mock adapter

```rust
// tests/common/mod.rs
pub struct MockAdapter {
    pub response: String,
}

#[async_trait::async_trait]
impl ProviderAdapter for MockAdapter {
    async fn complete(&self, _req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        Ok(CompletionResult {
            content: self.response.clone(),
            prompt_tokens: 10,
            completion_tokens: 5,
            finish_reason: "stop".to_string(),
        })
    }
    async fn stream(&self, _req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let content = self.response.clone();
        let stream = futures::stream::once(async move {
            Ok(bytes::Bytes::from(format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}]}}\n\n",
                content
            )))
        });
        Ok(Box::pin(stream))
    }
}
```

---

## Security Checklist (review before each release)

- [ ] No API keys logged at any log level
- [ ] Admin JWT secret is non-default in production config
- [ ] Dev seed key (`mr-dev-key`) produces a startup WARNING if still active
- [ ] All admin-mutating routes require `superadmin` role
- [ ] Pipeline hook outputs are only applied when `hook_permissions` table grants capability
- [ ] Request body size limit enforced (default 10MB)
- [ ] All DB queries use parameterised statements (sqlx enforces this — never use format strings in queries)
- [ ] bcrypt work factor ≥ 12 for admin passwords
- [ ] JWT uses HMAC-SHA256, secret ≥ 32 bytes
- [ ] `HttpOnly; SameSite=Strict` on admin session cookie

---

## Phase 10 — Quick Wins: Critical and High-Impact Low-Effort Features

_Source: LiteLLM gap analysis items 1–5. `docs/2026-04-02-litellm-feature-gap.md`_

**Goal:** Five high-value features each achievable in a day or less. No new subsystems required.

### 10.1 — Anthropic Messages API endpoint (`POST /v1/messages`)

**Files:**
- Create: `src/api/routes/messages.rs`
- Modify: `src/api/app.rs` (register route)
- Modify: `src/providers/anthropic.rs` (reuse existing adapter)

The Anthropic Messages API shape is `{"model":…, "messages":[…], "max_tokens":…}`. Translate it to `NormalizedRequest`, route through the existing pipeline (auth → policy → provider → cost logging), and translate the response back to Anthropic Messages API shape before returning.

This single route unblocks Claude Code: users set `ANTHROPIC_BASE_URL=http://localhost:8080` and `ANTHROPIC_API_KEY=<modelrouter-key>` and the proxy is transparent.

Key differences from `/v1/chat/completions`:
- `max_tokens` is required (not optional) in Anthropic's API
- Response shape uses `content: [{type: "text", text: "…"}]` not `choices[0].message.content`
- Streaming uses `event: content_block_delta` SSE format, not `data: {"choices":…}`

### 10.2 — Token limit enforcement

**Files:**
- Modify: `src/router/policy.rs`

The `limit_tokens` column already exists on `budget_rules`. Add a check in `PolicyEngine::check()`: query `SUM(tokens_in + tokens_out)` from `cost_ledger` for the user within the window; reject with 429 if at or above limit. Run this check in the same transaction as the dollar budget check. Add `tpm_limit` and `rpm_limit` fields (tokens/requests per minute) using the existing `rate_limit_state` sliding-window logic.

### 10.3 — Config-driven pricing table

**Files:**
- Modify: `src/config/schema.rs` (add `[[pricing]]` table)
- Modify: `src/router/cost.rs` (replace hardcoded map)
- Modify: `config.example.toml` (document the section)

Replace the hardcoded `HashMap` in `CostCalculator::new()` with a table loaded from config at startup. Default to the existing hardcoded values when no `[[pricing]]` section is present. Format:

```toml
[[pricing]]
model = "anthropic/claude-opus-4-5"
input_per_million  = 15.00
output_per_million = 75.00
```

Operators with enterprise pricing can override any model. Self-hosted models can be given a real cost.

### 10.4 — Fallback chain retry loop

**Files:**
- Modify: `src/router/engine.rs`
- Modify: `src/api/routes/completions.rs`

The `fallback_chains` config field is already parsed into `HashMap<String, Vec<String>>`. Implement `RequestRouter::fallback_chain(model: &str) -> Option<&[String]>`. In the completions handler, wrap the provider dispatch in a loop: on provider error (network timeout, 5xx), pop the next model from the chain and retry. On exhaustion, return the last error. Log each fallback attempt as a trace span event.

### 10.5 — Prometheus metrics endpoint

**Files:**
- Add dep: `prometheus` or `metrics` + `metrics-exporter-prometheus` to `Cargo.toml`
- Create: `src/api/routes/metrics.rs`
- Modify: `src/api/app.rs` (register `GET /metrics`)

Expose the same counters and histograms already recorded via OTel (requests total, tokens, cost, latency) as a Prometheus text-format scrape endpoint. When `--features otel` is active, bridge from the OTel `MeterProvider`. When not, use the `metrics` crate standalone. No auth required on `/metrics` (standard Prometheus convention; restrict via network policy).

### 10.6 — Tests

- `POST /v1/messages` with mock adapter: assert response shape, cost logged, budget enforced
- Token limit: assert 429 when `limit_tokens` exceeded; assert pass when under limit
- Custom pricing: assert cost calculated from config-provided rate, not hardcoded
- Fallback: mock primary returning 503; assert request completed via fallback model
- `/metrics`: assert Prometheus text format, assert request counter increments

**Deliverable:** Claude Code works natively. Token limits enforced. Prometheus scrape target live. Fallback chains operational.

---

## Phase 11 — Core Expansion: High-Impact Medium-Effort Features

_Source: LiteLLM gap analysis items 6–12._

**Goal:** Caching, embeddings, per-key budgets, Azure and Bedrock adapters, real load balancing.

### 11.1 — Complexity router

**Files:**
- Modify: `src/router/engine.rs`
- Modify: `src/config/schema.rs`

Add a `[routing.complexity]` config block:

```toml
[routing.complexity]
enabled         = true
simple_model    = "openai/gpt-4o-mini"
threshold_chars = 500          # prompts shorter than this go to simple_model
```

In `RequestRouter::resolve()`, after alias/prefix/default resolution, if complexity routing is enabled and the total prompt character count is below threshold, override the resolved model with `simple_model`. Record `model.complexity_override = true` as a span attribute.

### 11.2 — Exact-match response cache

**Files:**
- Create: `src/cache/mod.rs`, `src/cache/memory.rs`, `src/cache/redis.rs`
- Modify: `src/api/routes/completions.rs`
- Modify: `src/config/schema.rs` (`[cache]` section)
- Modify: `Cargo.toml` (`redis` optional dep behind `cache` feature)

Cache key: SHA-256 of `(model, messages JSON, temperature, top_p, max_tokens)`. On hit, return cached response with `x-litellm-cache-key` header. On miss, execute normally and write result to cache with configured TTL.

Backends: in-memory LRU (default, no deps) or Redis (`[cache] backend = "redis"`, `url = "redis://…"`).

Streaming responses: cache the fully assembled response after the stream closes; subsequent identical requests get the non-streamed cached version.

### 11.3 — Semantic cache (optional)

Extend the cache layer with an optional Qdrant backend. On miss: embed the user prompt using the configured embedding model, query Qdrant for nearest neighbours above a similarity threshold, return the closest cached response if found. Gated behind `[cache] semantic = true` and requires `backend = "redis"` (Qdrant stores vectors; Redis stores response payloads by vector ID).

### 11.4 — Embeddings endpoint (`POST /v1/embeddings`)

**Files:**
- Create: `src/api/routes/embeddings.rs`
- Modify: `src/providers/adapter.rs` (add `embed()` to `ProviderAdapter` trait)
- Modify: `src/providers/openai_compat.rs`, `src/providers/anthropic.rs`

Request shape: `{"model": "text-embedding-3-small", "input": "…"}`. Route through auth and cost logging. Return OpenAI-compatible response `{"data": [{"embedding": […], "index": 0}], …}`. Add per-million-token pricing to the pricing table for embedding models.

### 11.5 — Per-key budgets

**Files:**
- Create: `migrations/003_api_keys.sql`
- Modify: `src/db/models.rs`, `src/db/repositories/`
- Modify: `src/router/policy.rs`
- Modify: `src/api/auth.rs`
- Modify: `src/api/admin/routes.rs` (key CRUD)

Add `api_keys` table:

```sql
CREATE TABLE api_keys (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id),
    key_hash    TEXT NOT NULL UNIQUE,
    label       TEXT,
    expires_at  TEXT,
    metadata    TEXT DEFAULT '{}',
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TEXT NOT NULL
);
```

`BudgetRule` gains a nullable `api_key_id` FK alongside the existing `user_id`. Policy engine checks key-level budget before user-level budget. Auth extractor resolves to `(User, ApiKey)` pair; both are available to the policy engine and cost logger.

### 11.6 — Azure OpenAI adapter

**Files:**
- Create: `src/providers/azure.rs`
- Modify: `src/providers/registry.rs`
- Modify: `config.example.toml`

Azure OpenAI uses the same JSON shape as OpenAI but with a different URL structure: `https://<resource>.openai.azure.com/openai/deployments/<deployment>/chat/completions?api-version=2024-02-01`. Auth header is `api-key: …` not `Authorization: Bearer …`. Configuration:

```toml
[providers.azure]
api_key      = "…"
resource     = "my-resource"
deployment   = "gpt-4o"
api_version  = "2024-02-01"
```

### 11.7 — AWS Bedrock adapter

**Files:**
- Create: `src/providers/bedrock.rs`
- Add dep: `aws-sigv4`, `aws-credential-types` to `Cargo.toml` (behind `bedrock` feature)

Bedrock uses SigV4 request signing. The Converse API (`POST https://bedrock-runtime.<region>.amazonaws.com/model/<model-id>/converse`) accepts OpenAI-like chat structure and returns a unified response shape. Credentials from environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) or config.

### 11.8 — Load balancer

**Files:**
- Create: `src/router/load_balancer.rs`
- Modify: `src/router/engine.rs`
- Modify: `src/config/schema.rs` (`[[deployments]]` pool config)

Introduce a `Deployment` type: a named, routeable endpoint with health state and latency history. `ProviderRegistry` stores `Vec<Deployment>` per logical model name. `LoadBalancer` selects a deployment using the configured strategy:

- `round_robin` — atomic counter mod pool size
- `weighted` — weighted random selection
- `lowest_latency` — select deployment with lowest p95 from recent history

Deployment health state: `Healthy | Cooldown(until: Instant)`. Failed deployments enter cooldown; the circuit breaker (Phase 12) manages this transition.

### 11.9 — Tests

Full test coverage for all Phase 11 features. Cache tests use an in-memory backend. Azure and Bedrock tests use mock HTTP servers. Load balancer tests assert that a weighted distribution converges within ±5% of target weights over 1000 requests.

**Deliverable:** Cache reduces provider spend. Embeddings proxied. Azure and Bedrock unblocked. Load balanced across pools. Per-key billing granularity available.

---

## Phase 12 — Provider Expansion and Polish: Medium-Impact Low-Effort Features

_Source: LiteLLM gap analysis items 13–21._

**Goal:** Extend provider coverage and add nine targeted quality-of-life improvements.

### 12.1 — Additional provider adapters

New providers all use the existing `OpenAICompatAdapter` with a different base URL and API key header:

| Provider | Base URL | Notes |
|----------|----------|-------|
| Groq | `https://api.groq.com/openai/v1` | Standard Bearer auth |
| Mistral | `https://api.mistral.ai/v1` | Standard Bearer auth |
| DeepSeek | `https://api.deepseek.com/v1` | Standard Bearer auth |
| OpenRouter | `https://openrouter.ai/api/v1` | Adds `HTTP-Referer` and `X-Title` headers |

Config example for each follows the existing `[providers.<name>]` pattern. Pricing entries for each provider's model catalogue added to the default pricing table.

### 12.2 — Circuit breaker

**Files:** Modify `src/router/load_balancer.rs`

When a deployment returns a 5xx or times out, increment a per-deployment failure counter. When the counter reaches `circuit_breaker.failure_threshold` (default 5) within a rolling 60-second window, set the deployment state to `Cooldown(until: now + circuit_breaker.cooldown_secs)`. During cooldown, skip the deployment in selection. After cooldown, allow one probe request; on success restore to `Healthy`, on failure extend cooldown.

### 12.3 — IP-based rate limiting

**Files:** Modify `src/router/policy.rs`, `src/api/routes/completions.rs`

Extract `X-Forwarded-For` or `peer_addr` from the request; use it as an additional dimension in the `rate_limit_state` key alongside `user_id`. Add `ip_rpm_limit` to `budget_rules` (nullable; if null, no IP limit). Useful as a defence-in-depth measure independent of user identity.

### 12.4 — Concurrent request limiter

**Files:** Modify `src/api/app.rs`, `src/api/routes/completions.rs`

Add `Arc<Semaphore>` per user to `AppState` (lazy init via `DashMap<i64, Arc<Semaphore>>`). The semaphore permits count comes from the user's `max_parallel_requests` budget rule field. Acquire a permit before dispatching to the provider; release on response completion or error. Return 429 with `Retry-After: 1` if the semaphore is full.

### 12.5 — Spend reset API

`POST /admin/api/spend/reset` with body `{"user": "alice", "window": "monthly"}`. Updates `cost_ledger` by deleting rows within the window for the user, or sets a `reset_at` sentinel in `budget_rules` that the policy engine treats as the window start. Admin-only; logged in the audit trail.

### 12.6 — Per-tag budget rules

Extend `api_keys` with a `tags TEXT DEFAULT '[]'` JSON array column. Extend `budget_rules` with a nullable `tag TEXT` column. The policy engine evaluates tag-level rules before user-level rules: if the key has tag `project-alpha` and a `project-alpha` budget rule exists, apply it.

### 12.7 — Spend log cold storage

**Files:** Create `src/background/cold_storage.rs`

Background task runs daily: finds `cost_ledger` rows older than `cold_storage.retention_days` (default 90), serialises them as newline-delimited JSON, uploads to `s3://<bucket>/cost_ledger/YYYY/MM/DD.ndjson.gz`, then deletes the archived rows. Requires `[cold_storage]` config section with `bucket`, `region`, and AWS credentials. Gated behind `--features s3`.

### 12.8 — Anthropic prompt caching

**Files:** Modify `src/providers/anthropic.rs`

Before sending a request to Anthropic, if `[cache.anthropic_prompt_caching]` is enabled and the system prompt exceeds `min_tokens` (default 1024), inject `"cache_control": {"type": "ephemeral"}` on the last system message. Anthropic charges 10% of normal input token price for cache hits. Record a `prompt_cache_hit` span attribute from the response's `cache_read_input_tokens` field.

### 12.9 — Key TTL and auto-rotation

Extend `api_keys` with `expires_at TEXT` and `auto_rotate_days INTEGER`. Background job runs hourly: finds keys where `expires_at <= now()`, generates a new key, updates the record, logs the rotation to `audit_log`, and (if a webhook is configured) POSTs the new key to `rotation_webhook_url`. CLI: `modelrouter user rotate-key <name> --days 30` sets `auto_rotate_days`.

### 12.10 — Tests

**Deliverable:** Groq, Mistral, DeepSeek, OpenRouter available. Circuit breaker prevents timeout cascades. Nine quality improvements shipped with full test coverage.

---

## Phase 13 — Enterprise Readiness: Medium-Impact Medium-Effort Features

_Source: LiteLLM gap analysis items 22–26 and 29._

**Goal:** Hot-reload, guardrails, LLM observability callbacks, session rate limits, K8s Helm chart.

### 13.1 — Config hot-reload

**Files:** Create `src/background/config_sync.rs`; modify `src/api/app.rs`

Move the model/deployment list out of the static config file and into a `deployments` DB table that `modelrouter serve` populates from config on startup and subsequently syncs from every 10 seconds. Admin API gains `POST /admin/api/deployments` to add/remove deployments at runtime. `AppState` holds `Arc<RwLock<DeploymentPool>>` instead of a static registry.

### 13.2 — Guardrail framework

**Files:** Create `src/guardrails/mod.rs`, `src/guardrails/presidio.rs`

Define `Guardrail` trait:

```rust
pub trait Guardrail: Send + Sync {
    async fn check_request(&self, req: &NormalizedRequest) -> Result<GuardrailDecision>;
    async fn check_response(&self, resp: &CompletionResult) -> Result<GuardrailDecision>;
}
```

`GuardrailDecision` is `Allow | Block(reason: String) | Redact(modified: String)`. Wire guardrails into the completions handler between policy check and provider dispatch (pre-call) and between provider response and client return (post-call).

First implementation: Presidio HTTP client. Presidio runs as a sidecar; the guardrail POSTs request text to `http://presidio-analyzer/analyze` and, if PII entities are found above a confidence threshold, either blocks or redacts based on config.

### 13.3 — LangFuse callback

**Files:** Create `src/callbacks/langfuse.rs`

After request completion, fire an async task that POSTs a LangFuse trace to `https://cloud.langfuse.com` (or a self-hosted endpoint). Payload includes: `trace_id`, `user_id`, `model`, `prompt`, `completion`, `latency_ms`, `cost_usd`, `tokens`. Enabled via `[callbacks.langfuse]` config with `public_key`, `secret_key`, `host`.

### 13.4 — LangSmith callback

**Files:** Create `src/callbacks/langsmith.rs`

Same interface as LangFuse, targeting the LangSmith API. Payload format follows LangSmith's run creation schema. Enabled via `[callbacks.langsmith]` with `api_key`, `project`.

### 13.5 — Session-based rate limits

Extract `X-Session-Id` header from requests. Maintain per-session TPM/RPM counters in the rate limit state table alongside per-user counters. `budget_rules` gains `session_tpm_limit` and `session_rpm_limit` fields. This prevents a single agent loop from exhausting a user's per-minute quota before their interactive requests can run.

### 13.6 — Responses API passthrough (`POST /v1/responses`)

The OpenAI Responses API replaces function calling in newer SDKs. It uses the same auth and routing as `/v1/chat/completions`. The request and response shapes differ (tool use via `computer_use_preview` and structured output schemas). Implement as a near-passthrough: auth → policy → forward verbatim to the OpenAI/Anthropic endpoint → log tokens and cost from the response's usage field.

### 13.7 — Kubernetes Helm chart

**Files:** Create `deploy/helm/modelrouter/` (Chart.yaml, values.yaml, templates/)

Minimum viable chart includes: `Deployment`, `Service` (ClusterIP + optional LoadBalancer), `ConfigMap` (config.toml), `Secret` (provider API keys), `HorizontalPodAutoscaler` (CPU/memory), liveness probe (`GET /health`), readiness probe (`GET /health`), optional `PersistentVolumeClaim` for SQLite data volume. Values file exposes replicas, resource limits, image tag, and key overrides.

### 13.8 — Tests

**Deliverable:** Deployable on Kubernetes via `helm install`. PII guardrails active. LangFuse/LangSmith receive traces. Session limits prevent runaway agent loops.

---

## Phase 14 — Advanced Platform: Medium/Low-Impact Higher-Effort Features

_Source: LiteLLM gap analysis items 27–28 and 30–33._

**Goal:** MCP support, declarative policy engine, batch API, request queuing, and additional modalities.

### 14.1 — MCP server registry

**Files:** Create `src/mcp/mod.rs`, `migrations/004_mcp.sql`

```sql
CREATE TABLE mcp_servers (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL UNIQUE,
    url          TEXT NOT NULL,
    description  TEXT,
    enabled      BOOLEAN NOT NULL DEFAULT TRUE,
    access_groups TEXT DEFAULT '[]',   -- JSON array of group names
    created_at   TEXT NOT NULL
);
```

CRUD endpoints: `POST /v1/mcp/server`, `GET /v1/mcp/server`, `PUT /v1/mcp/server/{id}`, `DELETE /v1/mcp/server/{id}`. Access groups control which API keys can invoke which servers (checked against the `api_keys.tags` column).

### 14.2 — MCP tool calling passthrough

When a request to `/v1/chat/completions` includes `tools` sourced from an MCP server registered in the registry, the completions handler proxies the tool call execution request back through the MCP server and injects the result as a tool response message. Token usage for tool results is logged.

### 14.3 — Semantic MCP tool filtering

Before sending the tool list to the LLM, embed the user's last message, embed each tool's description, and select the top-K tools by cosine similarity above a threshold. Configurable via `[mcp.semantic_filter]`. Reduces context window consumption when MCP servers expose large tool catalogues.

### 14.4 — Declarative policy engine

**Files:** Create `src/policy/mod.rs`, `migrations/005_policies.sql`

Replace the hard-coded check sequence in `PolicyEngine` with a table of policy rules:

```sql
CREATE TABLE policies (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    scope      TEXT NOT NULL,   -- 'org' | 'team' | 'user' | 'key' | 'tag'
    scope_id   TEXT,            -- id or name within scope
    conditions TEXT NOT NULL,   -- JSON condition tree
    action     TEXT NOT NULL,   -- 'allow' | 'deny' | 'limit'
    params     TEXT,            -- JSON params for 'limit' action
    priority   INTEGER NOT NULL DEFAULT 100
);
```

Conditions are a small expression language: `{"field": "model", "op": "in", "value": ["claude-opus-4-5"]}` with `and`/`or` combinators. Policy resolution: load all rules for the request's org→team→user→key chain, sort by priority, evaluate in order, first matching rule wins.

### 14.5 — Policy CRUD endpoints

`POST /admin/api/policies`, `GET /admin/api/policies`, `GET /admin/api/policies/{id}`, `PUT /admin/api/policies/{id}`, `DELETE /admin/api/policies/{id}`, `GET /admin/api/policies/resolve?user=alice&model=claude-opus-4-5` (dry-run policy evaluation).

### 14.6 — Batch API

**Files:** Create `src/api/routes/batches.rs`, `migrations/006_batches.sql`, `src/background/batch_runner.rs`

`POST /v1/batches` — accept a JSONL file of requests, store in DB, return a `batch_id`. Background worker processes requests sequentially (or with limited concurrency), writing results to a JSONL results file. `GET /v1/batches/{id}` — status, progress, cost so far. `GET /v1/batches/{id}/results` — stream results file. Per-batch cost is calculated and logged to `cost_ledger` when all items complete.

### 14.7 — Request queue

When a user is rate-limited (429 from policy engine), instead of immediately returning the error, enqueue the request in a per-user `VecDeque<PendingRequest>` held in `AppState`. A background task drains the queue at the user's allowed rate and completes the pending requests. Maximum queue depth configurable (`[routing.queue_max_depth]`); requests beyond the limit are dropped with 429.

### 14.8 — Image generation

**Files:** Create `src/api/routes/images.rs`; extend `ProviderAdapter` with `generate_image()`

`POST /v1/images/generations` — route through auth and cost logging; dispatch to DALL-E 3 (OpenAI) or Stable Diffusion (Bedrock). Pricing per image (1024×1024 standard = $0.040, HD = $0.080). Response: `{"data": [{"url": "…"}]}`.

### 14.9 — Audio endpoints

**Files:** Create `src/api/routes/audio.rs`; extend `ProviderAdapter` with `transcribe()` and `speak()`

`POST /v1/audio/transcriptions` — multipart file upload, forward to Whisper, return transcript. `POST /v1/audio/speech` — text + voice params, forward to OpenAI TTS or Eleven Labs, return audio bytes. Pricing per minute (transcription) and per character (TTS).

### 14.10 — Tests

**Deliverable:** MCP tool calls route through modelrouter. Batch processing proxied with cost tracking. Image and audio modalities covered. Declarative policies replace hardcoded check sequence.

---

## Phase 15 — Enterprise Integrations: Low-Impact High-Effort Features

_Source: LiteLLM gap analysis items 34–40._

**Goal:** SSO, SCIM, billing hooks, shadow traffic, agent execution, vector stores, and realtime WebSocket.

### 15.1 — SSO / OIDC

**Files:** Create `src/api/admin/oidc.rs`; add dep `openidconnect`

Implement PKCE-based OIDC flow for the admin dashboard. Config: `[auth.oidc]` with `issuer_url`, `client_id`, `client_secret`, `redirect_uri`. On successful OIDC callback, look up or create an `admin_users` row by `email` claim; issue a session cookie. Role mapping from OIDC claims configurable via `[auth.oidc.role_claims]`.

### 15.2 — SCIM provisioning

`POST /scim/v2/Users` and `PATCH /scim/v2/Users/{id}` (RFC 7644). Provisioning events from the identity provider create/update/deactivate `users` rows. Bearer token auth on SCIM endpoints via a dedicated `scim_token` config value. Deactivation sets `enabled = false` rather than deleting (preserves cost history).

### 15.3 — Shadow traffic routing

Add `shadow_deployment` to the deployment config. When set, the completions handler fires a second, independent provider call to the shadow deployment after completing the primary call. The shadow response is discarded. Shadow call cost is logged separately (under a `shadow = true` tag) for cost comparison. Shadow calls do not affect the user's budget.

### 15.4 — Billing integrations

**Files:** Create `src/billing/stripe.rs`, `src/billing/lago.rs`

Background job runs hourly: sums `cost_ledger` rows since the last billing sync per user, and emits usage events to the configured billing provider. Stripe: `POST /v1/billing/meter_events` with `stripe_customer_id` from user metadata. Lago: `POST /api/v1/events` with `lago_external_subscription_id`. Configuration: `[billing.provider] = "stripe"` or `"lago"`.

### 15.5 — Agent endpoints

**Files:** Create `src/agents/mod.rs`, `migrations/007_agents.sql`

`agents` table: `id`, `name`, `system_prompt`, `tools JSON`, `model`, `session_tpm_limit`, `owner_user_id`. `POST /agents/{id}/execute` creates a session, runs the agentic loop (LLM call → tool dispatch → result injection → repeat until `stop_reason = "end_turn"`) with the configured `max_iterations` guard, and streams intermediate events as SSE. Each LLM call in the loop goes through the standard cost logging and budget enforcement path.

### 15.6 — Vector stores and RAG

**Files:** Create `src/vector/mod.rs`, `migrations/008_vector_stores.sql`

Thin management layer over an external vector store (Qdrant). `POST /v1/vector_stores` creates a Qdrant collection. `POST /v1/vector_stores/{id}/files` chunks and embeds an uploaded document, upserts vectors. `POST /v1/rag/retrieve` embeds the query and returns top-K chunks. Embedding model configurable; cost logged per embed call.

### 15.7 — Realtime WebSocket API

**Files:** Create `src/api/routes/realtime.rs`

`GET /v1/realtime` upgrades to WebSocket. Proxy the connection to OpenAI's Realtime API WebSocket endpoint, injecting auth headers and logging session start/end events. Token and cost accounting is done at session close using the `usage` event emitted by the OpenAI endpoint. Session budget limits enforced by closing the upstream connection when the limit is reached.

### 15.8 — Tests

**Deliverable:** Full LiteLLM feature parity. Enterprise identity management automated. Billing platforms connected. All request modalities covered.

- **Prompt search and tagging** [Keith's idea] — full-text search over stored prompts via SQLite FTS5. Tag prompts from the dashboard or CLI for categorisation and cost attribution.
