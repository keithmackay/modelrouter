-- The kind of call each prompt-log and ledger row records. See migrations/037_call_kind.sql.
ALTER TABLE prompts ADD COLUMN IF NOT EXISTS call_kind TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE cost_ledger ADD COLUMN IF NOT EXISTS call_kind TEXT NOT NULL DEFAULT 'unknown';
CREATE INDEX IF NOT EXISTS idx_prompts_call_kind ON prompts(call_kind, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_call_kind ON cost_ledger(call_kind, created_at);
