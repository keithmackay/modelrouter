-- The request fingerprint on each ledger row. See migrations/039_request_fingerprint.sql.
ALTER TABLE cost_ledger ADD COLUMN IF NOT EXISTS request_fingerprint TEXT;
CREATE INDEX IF NOT EXISTS idx_cost_ledger_request_fingerprint
    ON cost_ledger(request_fingerprint, created_at)
    WHERE request_fingerprint IS NOT NULL;
