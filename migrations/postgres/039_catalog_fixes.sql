-- Widen INTEGER columns the repositories decode as i64. SQLite's INTEGER is
-- 64-bit, so the shared models read these as i64; Postgres INTEGER is INT4
-- and sqlx refuses to decode it into an i64. Re-running is a no-op.
ALTER TABLE model_failovers ALTER COLUMN priority TYPE BIGINT;
ALTER TABLE budget_rules    ALTER COLUMN rate_rpm TYPE BIGINT;
