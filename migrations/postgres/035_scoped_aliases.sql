-- Scoped alias overrides. See migrations/035_scoped_aliases.sql.
CREATE TABLE IF NOT EXISTS scoped_aliases (
    id         BIGSERIAL PRIMARY KEY,
    tag_key    TEXT NOT NULL,
    tag_value  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    target     TEXT NOT NULL,
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    expires_at BIGINT NOT NULL DEFAULT 0,
    created_by TEXT,
    created_at TEXT NOT NULL DEFAULT (now()::text),
    UNIQUE (tag_key, tag_value, alias)
);
CREATE INDEX IF NOT EXISTS idx_scoped_aliases_expires_at ON scoped_aliases (expires_at);
