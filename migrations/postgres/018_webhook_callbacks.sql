CREATE TABLE IF NOT EXISTS webhook_callbacks (
    id                  BIGSERIAL PRIMARY KEY,
    name                TEXT NOT NULL,
    url                 TEXT NOT NULL,
    events              TEXT NOT NULL DEFAULT '["completion"]',
    secret_header_name  TEXT,
    secret_header_value TEXT,
    enabled             BOOLEAN NOT NULL DEFAULT TRUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
