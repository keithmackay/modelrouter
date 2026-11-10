-- Columns the SQLite schema gained (004, 005, 006, 007, 008, 011) that the
-- Postgres history never added, plus the 020 membership indexes. The
-- repositories read and write all of them.
ALTER TABLE users        ADD COLUMN IF NOT EXISTS spend_reset_at TEXT;
ALTER TABLE api_keys     ADD COLUMN IF NOT EXISTS expires_at     TEXT;
ALTER TABLE api_keys     ADD COLUMN IF NOT EXISTS tag            TEXT;
ALTER TABLE api_keys     ADD COLUMN IF NOT EXISTS project        TEXT;
ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS max_concurrent BIGINT;
ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS tag            TEXT;

CREATE INDEX IF NOT EXISTS idx_group_memberships_group_id ON group_memberships(group_id);
CREATE INDEX IF NOT EXISTS idx_group_memberships_user_id  ON group_memberships(user_id);
