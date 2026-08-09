-- Response-cache metering: a cache hit is a real usage record with zero
-- provider cost. `saved_usd` records what the call would have cost, so the
-- saving is reportable alongside spend without double-counting it as spend.
ALTER TABLE cost_ledger ADD COLUMN cache_hit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cost_ledger ADD COLUMN saved_usd REAL NOT NULL DEFAULT 0;
