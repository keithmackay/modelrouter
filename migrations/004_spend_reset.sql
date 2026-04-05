-- migrations/004_spend_reset.sql
-- Non-destructive spend reset: track the timestamp from which spend is counted.
-- NULL means count from the beginning of time (no reset performed).
ALTER TABLE users ADD COLUMN spend_reset_at TEXT;
