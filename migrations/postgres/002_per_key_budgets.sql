-- migrations/postgres/002_per_key_budgets.sql
CREATE TABLE IF NOT EXISTS api_keys (
    id         BIGSERIAL PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key_hash   TEXT NOT NULL UNIQUE,
    label      TEXT,
    enabled    BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_api_keys_user ON api_keys(user_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);

ALTER TABLE budget_rules ADD COLUMN IF NOT EXISTS api_key_id BIGINT REFERENCES api_keys(id) ON DELETE CASCADE;
