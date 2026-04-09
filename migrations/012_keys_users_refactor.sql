-- Rebuild users table without legacy key columns and add email field.
-- SQLite does not support DROP COLUMN on UNIQUE columns, so we recreate the table.

CREATE TABLE users_new (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL UNIQUE,
    group_name     TEXT,
    email          TEXT,
    spend_reset_at TEXT,
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL,
    metadata       TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO users_new (id, name, group_name, spend_reset_at, enabled, created_at, metadata)
SELECT id, name, group_name, spend_reset_at, enabled, created_at, metadata FROM users;

DROP TABLE users;

ALTER TABLE users_new RENAME TO users;
