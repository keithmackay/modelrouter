-- Operator disable/enable of models and providers (issue #5).
-- See migrations/025_disable_metadata.sql.
ALTER TABLE models ADD COLUMN IF NOT EXISTS disabled_reason TEXT;
ALTER TABLE models ADD COLUMN IF NOT EXISTS disabled_by TEXT;
ALTER TABLE models ADD COLUMN IF NOT EXISTS disabled_at TEXT;

CREATE TABLE IF NOT EXISTS provider_states (
    provider        TEXT PRIMARY KEY,
    enabled         BOOLEAN NOT NULL DEFAULT true,
    disabled_reason TEXT,
    disabled_by     TEXT,
    disabled_at     TEXT,
    updated_at      TEXT NOT NULL DEFAULT (now()::text)
);
