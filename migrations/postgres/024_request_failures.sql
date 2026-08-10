-- Every request that FAILED, captured. See migrations/024_request_failures.sql
-- for the rationale; this is the PostgreSQL spelling of the same change.
CREATE TABLE IF NOT EXISTS request_failures (
    id            BIGSERIAL PRIMARY KEY,
    user_id       BIGINT REFERENCES users(id),
    api_key_id    BIGINT REFERENCES api_keys(id),
    endpoint      TEXT NOT NULL,
    request_model TEXT NOT NULL,
    routed_model  TEXT,
    provider      TEXT,
    stage         TEXT NOT NULL,
    status_code   INTEGER,
    error_message TEXT NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 1,
    latency_ms    BIGINT,
    project       TEXT,
    attribution_correlation_id TEXT,
    attribution_tags TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_request_failures_created
    ON request_failures (created_at);

CREATE INDEX IF NOT EXISTS idx_request_failures_stage
    ON request_failures (stage, created_at);

CREATE INDEX IF NOT EXISTS idx_request_failures_correlation
    ON request_failures (attribution_correlation_id, created_at);
