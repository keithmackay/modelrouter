-- The kind of call each prompt-log and ledger row records (chat, embedding,
-- search, image, audio, decision, judgment, health), so spend and usage can be
-- reported per kind: a decision is not a chat completion on the same model.
-- Additive: rows written before this column existed read 'unknown'.
ALTER TABLE prompts ADD COLUMN call_kind TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE cost_ledger ADD COLUMN call_kind TEXT NOT NULL DEFAULT 'unknown';
CREATE INDEX IF NOT EXISTS idx_prompts_call_kind ON prompts(call_kind, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_call_kind ON cost_ledger(call_kind, created_at);
