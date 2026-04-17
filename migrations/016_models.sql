-- DB-driven model registry
CREATE TABLE IF NOT EXISTS models (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    provider   TEXT NOT NULL,
    name       TEXT NOT NULL,
    alias      TEXT UNIQUE,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Failover chains: primary_model -> ordered list of fallback models
CREATE TABLE IF NOT EXISTS model_failovers (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    primary_model  TEXT NOT NULL,
    fallback_model TEXT NOT NULL,
    priority       INTEGER NOT NULL DEFAULT 0,
    UNIQUE(primary_model, fallback_model)
);
