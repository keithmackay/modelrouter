-- GUI-managed runtime settings (issue #4).
-- Key-value store for settings an admin edits in the dashboard rather than in
-- config.toml. Values are JSON blobs per section (e.g. key 'storage' holds the
-- serialized StorageConfig). A DB row overrides the config-file value for that
-- section; absence of a row means "use config.toml / built-in defaults".
CREATE TABLE IF NOT EXISTS app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
