-- Controlled experiments (spec §7a) and per-run outcome feedback (§7a, §11).
--
-- An experiment is a named set of variants. Each variant is an overlay: a map
-- from a requested model name to the target it is redirected to. Targets are
-- pinned at creation — `variants` stores, for every target, the provider and
-- model the target expression resolved to at that moment — so an alias edit
-- afterwards changes ordinary traffic, never an active experiment.
--
--   variants: {"<label>": {"<requested model>": {"target": "<expression>",
--                                                "provider": "<p>", "model": "<m>"}}}
--
-- `allowed_user_ids` is a JSON list of user ids; empty means every key may
-- bind. `expires_at` is unix seconds because it is compared arithmetically
-- (Rust and the auto-close statement); 0 means never. Every other timestamp
-- stays RFC3339 text like the rest of the schema. `expires_at` and
-- `content_retention_days` deliberately have NO default: both are required at
-- creation at every layer, and 0 is the explicit spelling of "never".
-- `feed_learning` is stored and returned only; nothing reads it yet.
CREATE TABLE IF NOT EXISTS experiments (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    name                   TEXT NOT NULL UNIQUE,
    variants               TEXT NOT NULL,
    allowed_user_ids       TEXT NOT NULL DEFAULT '[]',
    status                 TEXT NOT NULL,
    feed_learning          INTEGER NOT NULL DEFAULT 0,
    expires_at             INTEGER NOT NULL,
    created_at             TEXT NOT NULL,
    closed_at              TEXT,
    retain_content         INTEGER NOT NULL DEFAULT 0,
    content_retention_days INTEGER NOT NULL
);

-- One outcome per run per user. A run is keyed by (user_id, correlation id):
-- correlation ids are caller-chosen, so two keys can carry the same one, and
-- the user half stops one key's outcome attaching to another key's run. The
-- experiment columns are a snapshot of the run's earliest stamped ledger row.
-- `note` is bounded metadata, never prompt or response content.
CREATE TABLE IF NOT EXISTS run_outcomes (
    user_id                    INTEGER NOT NULL REFERENCES users(id),
    attribution_correlation_id TEXT NOT NULL,
    outcome                    TEXT NOT NULL,
    score                      REAL,
    rating                     INTEGER,
    note                       TEXT,
    experiment_id              INTEGER,
    experiment_variant         TEXT,
    created_at                 TEXT NOT NULL,
    updated_at                 TEXT NOT NULL,
    PRIMARY KEY (user_id, attribution_correlation_id)
);

CREATE INDEX IF NOT EXISTS idx_run_outcomes_experiment
    ON run_outcomes (experiment_id);

-- Every row written for a bound request carries the experiment and variant,
-- so results never depend on re-deriving the binding from headers after the fact.
ALTER TABLE prompts ADD COLUMN experiment_id INTEGER;
ALTER TABLE prompts ADD COLUMN experiment_variant TEXT;
ALTER TABLE cost_ledger ADD COLUMN experiment_id INTEGER;
ALTER TABLE cost_ledger ADD COLUMN experiment_variant TEXT;
ALTER TABLE request_failures ADD COLUMN experiment_id INTEGER;
ALTER TABLE request_failures ADD COLUMN experiment_variant TEXT;

-- Set when the provider reported no usage and the token counts were estimated
-- locally, so aggregates can say how much of a total is measured.
ALTER TABLE cost_ledger ADD COLUMN tokens_estimated INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_prompts_experiment
    ON prompts (experiment_id, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_experiment
    ON cost_ledger (experiment_id, created_at);
CREATE INDEX IF NOT EXISTS idx_request_failures_experiment
    ON request_failures (experiment_id, created_at);

-- Runs are joined to prompt rows by correlation id; the ledger already has
-- this index (023), prompts did not.
CREATE INDEX IF NOT EXISTS idx_prompts_correlation
    ON prompts (attribution_correlation_id, created_at);

-- Feedback looks a run up by (user, correlation id); correlation ids are
-- caller-chosen and may repeat across users, so the user leads the index.
CREATE INDEX IF NOT EXISTS idx_cost_ledger_user_correlation
    ON cost_ledger (user_id, attribution_correlation_id);
CREATE INDEX IF NOT EXISTS idx_request_failures_user_correlation
    ON request_failures (user_id, attribution_correlation_id);
