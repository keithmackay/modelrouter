-- Track when an API key was disabled (for display in admin UI)
ALTER TABLE api_keys ADD COLUMN disabled_at TEXT;
