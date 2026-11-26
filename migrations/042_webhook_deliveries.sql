-- Usage-webhook deliveries waiting to be sent. Written in the same
-- process as the cost-ledger row, then claimed by any replica under a
-- lease, so a crash or a rollout no longer drops a retry that lived only
-- in memory. Expand-only: a binary that does not know this table never
-- reads it.
--
-- next_attempt_at is NULL once the row is delivered or given up.
-- delivered_at is set only on success; those rows are deleted after the
-- retention window. A row that was given up keeps last_error and stays.
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    webhook_id      INTEGER NOT NULL,
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
