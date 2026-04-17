-- DB-driven model registry
CREATE TABLE IF NOT EXISTS models (
    id         BIGSERIAL PRIMARY KEY,
    provider   TEXT NOT NULL,
    name       TEXT NOT NULL,
    alias      TEXT UNIQUE,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TEXT NOT NULL DEFAULT (to_char(NOW() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"+00:00"'))
);

-- Failover chains: primary_model -> ordered list of fallback models
CREATE TABLE IF NOT EXISTS model_failovers (
    id             BIGSERIAL PRIMARY KEY,
    primary_model  TEXT NOT NULL,
    fallback_model TEXT NOT NULL,
    priority       INTEGER NOT NULL DEFAULT 0,
    UNIQUE(primary_model, fallback_model)
);
