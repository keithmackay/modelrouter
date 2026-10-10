-- Scoped alias overrides: an alias -> provider/model mapping that applies
-- only to requests whose attribution tags carry `tag_key = tag_value`.
-- Resolution is: experiment overlay, then a scoped override, then the global
-- aliases. Targets are pinned at write time (provider and model), so a later
-- edit of a global alias never changes an override. `expires_at` is unix
-- seconds, 0 meaning never; expired rows are ignored and then deleted by the
-- lifecycle tick.
CREATE TABLE IF NOT EXISTS scoped_aliases (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    tag_key    TEXT NOT NULL,
    tag_value  TEXT NOT NULL,
    alias      TEXT NOT NULL,
    target     TEXT NOT NULL,
    provider   TEXT NOT NULL,
    model      TEXT NOT NULL,
    expires_at INTEGER NOT NULL DEFAULT 0,
    created_by TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (tag_key, tag_value, alias)
);
CREATE INDEX IF NOT EXISTS idx_scoped_aliases_expires_at ON scoped_aliases (expires_at);
