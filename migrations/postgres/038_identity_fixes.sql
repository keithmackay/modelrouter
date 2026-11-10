-- migrations/postgres/038_identity_fixes.sql
-- SQLite INTEGER is 64-bit and the repositories decode these columns as i64;
-- INTEGER (INT4) here made every non-NULL read fail to decode.
ALTER TABLE api_keys         ALTER COLUMN session_window_secs TYPE BIGINT;
ALTER TABLE rate_limit_state ALTER COLUMN request_count       TYPE BIGINT;
