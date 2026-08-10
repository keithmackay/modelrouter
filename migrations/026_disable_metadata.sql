-- Operator disable/enable of models and providers (issue #5).
-- `models.enabled` already existed; these columns record *who* took a model out
-- of rotation, *when*, and *why*, so the dashboard and audit trail can show it.
ALTER TABLE models ADD COLUMN disabled_reason TEXT;
ALTER TABLE models ADD COLUMN disabled_by TEXT;
ALTER TABLE models ADD COLUMN disabled_at TEXT;

-- Whole-provider disable. A row exists only once a provider has been toggled;
-- absence means enabled, so providers defined only in config.toml need no seed.
CREATE TABLE IF NOT EXISTS provider_states (
    provider        TEXT PRIMARY KEY,
    enabled         INTEGER NOT NULL DEFAULT 1,
    disabled_reason TEXT,
    disabled_by     TEXT,
    disabled_at     TEXT,
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
