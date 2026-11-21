-- A fingerprint of the request each ledger row records: the hash of the
-- normalised request body the response cache keys on, computed for every call
-- whether or not the cache applied to it. Two rows with one fingerprint are
-- one request made twice, so repeated work can be reported without keeping any
-- request content (`report duplicates`, GET /admin/api/usage/duplicates).
-- Additive: rows written before this column existed stay NULL and are left out
-- of the duplicates report.
ALTER TABLE cost_ledger ADD COLUMN request_fingerprint TEXT;
CREATE INDEX IF NOT EXISTS idx_cost_ledger_request_fingerprint
    ON cost_ledger(request_fingerprint, created_at)
    WHERE request_fingerprint IS NOT NULL;
