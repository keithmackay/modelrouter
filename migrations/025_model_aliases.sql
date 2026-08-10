-- Runtime-managed model aliases (issue #9).
-- Distinct from `models.alias`, which is a convenience alias attached to a
-- registered model row. Rows here are free-form alias -> target mappings that
-- an operator can create/update/delete at runtime through the admin surfaces,
-- and they override both `models.alias` and config-file `routing.model_aliases`.
CREATE TABLE IF NOT EXISTS model_aliases (
    alias      TEXT PRIMARY KEY,
    target     TEXT NOT NULL,
    created_by TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
