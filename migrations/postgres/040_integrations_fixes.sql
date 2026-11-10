-- webhook_callbacks.created_at was TIMESTAMPTZ, unlike every other timestamp
-- column in the schema: the repository writes and reads it as an RFC 3339
-- string, which Postgres refuses in both directions. Convert it to TEXT,
-- keeping existing values in the same RFC 3339 form. Guarded so a rerun, or
-- a database already converted, is a no-op.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'webhook_callbacks'
          AND column_name = 'created_at'
          AND data_type = 'timestamp with time zone'
    ) THEN
        ALTER TABLE webhook_callbacks ALTER COLUMN created_at DROP DEFAULT;
        ALTER TABLE webhook_callbacks ALTER COLUMN created_at TYPE TEXT
            USING to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"');
        ALTER TABLE webhook_callbacks ALTER COLUMN created_at
            SET DEFAULT (to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'));
    END IF;
END
$$;
