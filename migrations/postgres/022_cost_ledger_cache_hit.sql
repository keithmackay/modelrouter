-- Response-cache metering: a cache hit is a real usage record with zero
-- provider cost. `saved_usd` records what the call would have cost, so the
-- saving is reportable alongside spend without double-counting it as spend.
ALTER TABLE cost_ledger ADD COLUMN cache_hit BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE cost_ledger ADD COLUMN saved_usd DOUBLE PRECISION NOT NULL DEFAULT 0;
