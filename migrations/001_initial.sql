-- migrations/001_initial.sql
CREATE TABLE IF NOT EXISTS users (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    api_key     TEXT NOT NULL UNIQUE,
    api_key_old TEXT,
    api_key_old_expires_at TEXT,
    group_name  TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS admin_users (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL UNIQUE,
    password_hash   TEXT NOT NULL,
    role            TEXT NOT NULL DEFAULT 'viewer',
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL,
    last_login_at   TEXT
);

CREATE TABLE IF NOT EXISTS budget_rules (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER REFERENCES users(id) ON DELETE CASCADE,
    group_name   TEXT,
    window       TEXT NOT NULL,
    limit_usd    REAL,
    limit_tokens INTEGER,
    model_allow  TEXT NOT NULL DEFAULT '[]',
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
    messages          TEXT NOT NULL,
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
    capability TEXT NOT NULL,
    granted_by INTEGER REFERENCES admin_users(id),
    granted_at TEXT NOT NULL,
    UNIQUE(hook_name, capability)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_id   INTEGER REFERENCES admin_users(id),
    actor_name TEXT NOT NULL,
    action     TEXT NOT NULL,
    target     TEXT,
    before_json TEXT,
    after_json  TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS hook_metrics (
    hook_name   TEXT NOT NULL,
    invoked_at  TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    success     INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_prompts_user_created ON prompts(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_user_created ON cost_ledger(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_audit_log_actor ON audit_log(actor_id, created_at);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
