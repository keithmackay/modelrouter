-- Columns the SQLite schema gained in 004-008 and 011 that the PostgreSQL
-- history never added, plus the 020 membership indexes. The repositories read
-- and write all of them.
--
-- This replaces two files that both claimed version 35 (backend_parity and
-- schema_parity) alongside 035_scoped_aliases. Every statement is guarded, so
-- a database that already ran either of them is left as it is.
ALTER TABLE users        ADD COLUMN IF NOT EXISTS spend_reset_at TEXT;
ALTER TABLE api_keys     ADD COLUMN IF NOT EXISTS expires_at     TEXT;
ALTER TABLE api_keys     ADD COLUMN IF NOT EXISTS tag            TEXT;
ALTER TABLE api_keys     ADD COLUMN IF NOT EXISTS project        TEXT;
ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS max_concurrent BIGINT;
ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS tag            TEXT;

CREATE INDEX IF NOT EXISTS idx_group_memberships_group_id ON group_memberships(group_id);
CREATE INDEX IF NOT EXISTS idx_group_memberships_user_id  ON group_memberships(user_id);
