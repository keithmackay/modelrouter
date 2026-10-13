-- Model capabilities the router learned from provider rejections.
-- One row per exact routed model id (provider segments stripped, lowercased,
-- any `@version` kept): when a provider rejected a request because of a
-- parameter, the router retried without it and recorded the rejection here,
-- so the parameter is never sent to that model again. Config
-- `[[model_capabilities]]` entries outrank these rows; these outrank the
-- built-in table. An operator deletes a row once the provider restores support.
CREATE TABLE IF NOT EXISTS learned_model_capabilities (
    model                TEXT PRIMARY KEY,
    supports_temperature INTEGER NOT NULL,
    error                TEXT NOT NULL,
    learned_at           TEXT NOT NULL DEFAULT (datetime('now'))
);
