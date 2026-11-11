-- contract-step: 0.1.0 widening only; every binary that reads budget_rules.max_concurrent decodes it as i64
--
-- The retired 035_backend_parity created budget_rules.max_concurrent as
-- INTEGER (INT4). A database that ran it before 042 keeps that type, because
-- 042's ADD COLUMN IF NOT EXISTS leaves an existing column alone, and sqlx
-- refuses to decode INT4 into the i64 the repository reads. Widen it. Guarded,
-- so a database that already has BIGINT is a no-op.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'budget_rules'
          AND column_name = 'max_concurrent'
          AND data_type = 'integer'
    ) THEN
        ALTER TABLE budget_rules ALTER COLUMN max_concurrent TYPE BIGINT;
    END IF;
END
$$;
