-- Columns the SQLite schema gained in 004-011 that the PostgreSQL set never
-- added, although the PostgreSQL repositories select and insert them. All
-- nullable, so this is expand-only.
ALTER TABLE users ADD COLUMN IF NOT EXISTS spend_reset_at TEXT;
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS expires_at TEXT;
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS tag TEXT;
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS project TEXT;
ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS max_concurrent INTEGER;
ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS tag TEXT;
