-- Caller-supplied request attribution. See migrations/023_request_attribution.sql
-- for the rationale; this is the PostgreSQL spelling of the same change.
ALTER TABLE prompts ADD COLUMN attribution_correlation_id TEXT;
ALTER TABLE prompts ADD COLUMN attribution_tags TEXT NOT NULL DEFAULT '{}';
ALTER TABLE cost_ledger ADD COLUMN attribution_correlation_id TEXT;
ALTER TABLE cost_ledger ADD COLUMN attribution_tags TEXT NOT NULL DEFAULT '{}';

CREATE INDEX IF NOT EXISTS idx_cost_ledger_correlation
    ON cost_ledger (attribution_correlation_id, created_at);
