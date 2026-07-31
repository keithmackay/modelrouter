-- migrations/014_groups.sql
-- Recreate users table without group_name (preserve all other columns and data)
CREATE TABLE users_new (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    email          TEXT,
    spend_reset_at TEXT,
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL,
    metadata       TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO users_new (id, name, email, spend_reset_at, enabled, created_at, metadata)
SELECT id, name, email, spend_reset_at, enabled, created_at, metadata FROM users;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

-- Groups table
CREATE TABLE groups (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    priority   INTEGER NOT NULL DEFAULT 0,
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- Group memberships (no UNIQUE constraint — re-add creates a new row)
CREATE TABLE group_memberships (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id   INTEGER NOT NULL REFERENCES groups(id),
    user_id    INTEGER NOT NULL REFERENCES users(id),
    joined_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    disabled_at TEXT
);
