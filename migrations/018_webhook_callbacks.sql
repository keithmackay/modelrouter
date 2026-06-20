CREATE TABLE IF NOT EXISTS webhook_callbacks (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    name                TEXT NOT NULL,
    url                 TEXT NOT NULL,
    events              TEXT NOT NULL DEFAULT '["completion"]',
    secret_header_name  TEXT,
    secret_header_value TEXT,
    enabled             INTEGER NOT NULL DEFAULT 1,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
