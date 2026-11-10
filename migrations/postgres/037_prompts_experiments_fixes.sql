-- request_failures.status_code and attempts were created as INTEGER, but the
-- repository reads and writes them as 64-bit like every other integer column
-- (the SQLite schema has a single INTEGER type). Widening is idempotent.
ALTER TABLE request_failures ALTER COLUMN status_code TYPE BIGINT;
ALTER TABLE request_failures ALTER COLUMN attempts    TYPE BIGINT;
