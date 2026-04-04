-- migrations/003_cost_ledger_api_key.sql
ALTER TABLE cost_ledger ADD COLUMN api_key_id INTEGER REFERENCES api_keys(id);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_key_created ON cost_ledger(api_key_id, created_at);
