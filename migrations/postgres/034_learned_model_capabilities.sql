-- Model capabilities the router learned from provider rejections.
-- See migrations/034_learned_model_capabilities.sql.
CREATE TABLE IF NOT EXISTS learned_model_capabilities (
    model                TEXT PRIMARY KEY,
    supports_temperature BOOLEAN NOT NULL,
    error                TEXT NOT NULL,
    learned_at           TEXT NOT NULL DEFAULT (now()::text)
);
