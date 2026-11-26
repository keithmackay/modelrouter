-- Same change as migrations/042_webhook_deliveries.sql.
-- PostgreSQL claims due rows with FOR UPDATE SKIP LOCKED; a lease
-- (locked_until) lets another replica retry a claim whose holder died.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id              BIGSERIAL PRIMARY KEY,
    webhook_id      BIGINT NOT NULL,
    event           TEXT NOT NULL,
    payload         TEXT NOT NULL,
    attempt         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    locked_by       TEXT,
    locked_until    TEXT,
    last_error      TEXT,
    delivered_at    TEXT,
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_due
    ON webhook_deliveries (next_attempt_at, id)
    WHERE delivered_at IS NULL AND next_attempt_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_delivered
    ON webhook_deliveries (delivered_at)
    WHERE delivered_at IS NOT NULL;
