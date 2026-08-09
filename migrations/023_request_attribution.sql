-- Caller-supplied request attribution.
--
-- `project` is a property of the API key; these two columns are properties of
-- the individual request, so a consuming app can attribute spend to its own
-- unit of work (an engagement, a job, a run) without minting a key per unit.
--
-- `attribution_tags` holds a small bounded JSON object ({} when absent) and
-- `attribution_correlation_id` a single opaque caller id. Both are denormalised
-- onto cost_ledger so cost queries never need to join back through prompts —
-- which matters because skip-log and cache-hit rows have prompt_id IS NULL.
ALTER TABLE prompts ADD COLUMN attribution_correlation_id TEXT;
ALTER TABLE prompts ADD COLUMN attribution_tags TEXT NOT NULL DEFAULT '{}';
ALTER TABLE cost_ledger ADD COLUMN attribution_correlation_id TEXT;
ALTER TABLE cost_ledger ADD COLUMN attribution_tags TEXT NOT NULL DEFAULT '{}';

CREATE INDEX IF NOT EXISTS idx_cost_ledger_correlation
    ON cost_ledger (attribution_correlation_id, created_at);
