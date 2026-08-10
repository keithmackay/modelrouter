-- Runtime-managed model aliases (issue #9). See migrations/024_model_aliases.sql.
CREATE TABLE IF NOT EXISTS model_aliases (
    alias      TEXT PRIMARY KEY,
    target     TEXT NOT NULL,
    created_by TEXT,
    created_at TEXT NOT NULL DEFAULT (now()::text),
    updated_at TEXT NOT NULL DEFAULT (now()::text)
);
