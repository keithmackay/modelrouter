-- migrations/postgres/001_initial.sql
-- PostgreSQL-compatible schema for modelrouter

CREATE TABLE IF NOT EXISTS users (
    id                      BIGSERIAL PRIMARY KEY,
    name                    TEXT NOT NULL UNIQUE,
    api_key                 TEXT NOT NULL UNIQUE,
    api_key_old             TEXT,
    api_key_old_expires_at  TEXT,
    group_name              TEXT,
    enabled                 BOOLEAN NOT NULL DEFAULT TRUE,
    created_at              TEXT NOT NULL,
    metadata                TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS admin_users (
    id              BIGSERIAL PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    password_hash   TEXT NOT NULL,
    role            TEXT NOT NULL DEFAULT 'viewer',
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TEXT NOT NULL,
    last_login_at   TEXT
);

CREATE TABLE IF NOT EXISTS budget_rules (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT REFERENCES users(id) ON DELETE CASCADE,
    group_name   TEXT,
    window       TEXT NOT NULL,
    limit_usd    DOUBLE PRECISION,
    limit_tokens BIGINT,
    model_allow  TEXT NOT NULL DEFAULT '[]',
    model_deny   TEXT NOT NULL DEFAULT '[]',
    rate_rpm     INTEGER,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id          BIGSERIAL PRIMARY KEY,
    user_id     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    external_id TEXT,
    project     TEXT,
    created_at  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS prompts (
    id                BIGSERIAL PRIMARY KEY,
    user_id           BIGINT NOT NULL REFERENCES users(id),
    session_id        BIGINT REFERENCES sessions(id),
    request_model     TEXT NOT NULL,
    routed_model      TEXT NOT NULL,
    provider          TEXT NOT NULL,
    messages          TEXT NOT NULL,
    response          TEXT,
    finish_reason     TEXT,
    prompt_tokens     BIGINT NOT NULL DEFAULT 0,
    completion_tokens BIGINT NOT NULL DEFAULT 0,
    cost_usd          DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    latency_ms        BIGINT,
    tags              TEXT NOT NULL DEFAULT '[]',
    project           TEXT,
    created_at        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS cost_ledger (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id),
    prompt_id  BIGINT NOT NULL REFERENCES prompts(id),
    model      TEXT NOT NULL,
    provider   TEXT NOT NULL,
    project    TEXT,
    tokens_in  BIGINT NOT NULL DEFAULT 0,
    tokens_out BIGINT NOT NULL DEFAULT 0,
    cost_usd   DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rate_limit_state (
    user_id       BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    window_key    TEXT NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, window_key)
);

CREATE TABLE IF NOT EXISTS hook_permissions (
    id         BIGSERIAL PRIMARY KEY,
    hook_name  TEXT NOT NULL,
    capability TEXT NOT NULL,
    granted_by BIGINT REFERENCES admin_users(id),
    granted_at TEXT NOT NULL,
    UNIQUE(hook_name, capability)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id          BIGSERIAL PRIMARY KEY,
    actor_id    BIGINT REFERENCES admin_users(id),
    actor_name  TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    before_json TEXT,
    after_json  TEXT,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS hook_metrics (
    hook_name   TEXT NOT NULL,
    invoked_at  TEXT NOT NULL,
    duration_ms BIGINT NOT NULL,
    success     BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE INDEX IF NOT EXISTS idx_prompts_user_created ON prompts(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_user_created ON cost_ledger(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_audit_log_actor ON audit_log(actor_id, created_at);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
