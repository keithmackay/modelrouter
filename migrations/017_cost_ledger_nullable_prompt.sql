-- Make prompt_id nullable in cost_ledger to support X-No-Log requests
-- where cost must still be tracked but no prompt row is created.
-- SQLite does not support ALTER COLUMN, so we recreate the table.
CREATE TABLE IF NOT EXISTS cost_ledger_new (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    prompt_id  INTEGER REFERENCES prompts(id),
    model      TEXT NOT NULL,
    provider   TEXT NOT NULL DEFAULT '',
    project    TEXT,
    tokens_in  INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    cost_usd   REAL NOT NULL DEFAULT 0,
    api_key_id INTEGER REFERENCES api_keys(id),
    created_at TEXT NOT NULL
);

INSERT INTO cost_ledger_new
    (id, user_id, prompt_id, model, provider, project, tokens_in, tokens_out, cost_usd, api_key_id, created_at)
SELECT id, user_id, prompt_id, model, provider, project, tokens_in, tokens_out, cost_usd, api_key_id, created_at
FROM cost_ledger;

DROP TABLE cost_ledger;
ALTER TABLE cost_ledger_new RENAME TO cost_ledger;
