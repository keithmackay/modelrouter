-- Controlled experiments and per-run outcome feedback. See
-- migrations/029_experiments.sql for the rationale; this is the PostgreSQL
-- spelling of the same change.
CREATE TABLE IF NOT EXISTS experiments (
    id                     BIGSERIAL PRIMARY KEY,
    name                   TEXT NOT NULL UNIQUE,
    variants               TEXT NOT NULL,
    allowed_user_ids       TEXT NOT NULL DEFAULT '[]',
    status                 TEXT NOT NULL,
    feed_learning          BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at             BIGINT NOT NULL,
    created_at             TEXT NOT NULL,
    closed_at              TEXT,
    retain_content         BOOLEAN NOT NULL DEFAULT FALSE,
    content_retention_days BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS run_outcomes (
    user_id                    BIGINT NOT NULL REFERENCES users(id),
    attribution_correlation_id TEXT NOT NULL,
    outcome                    TEXT NOT NULL,
    score                      DOUBLE PRECISION,
    rating                     BIGINT,
    note                       TEXT,
    experiment_id              BIGINT,
    experiment_variant         TEXT,
    created_at                 TEXT NOT NULL,
    updated_at                 TEXT NOT NULL,
    PRIMARY KEY (user_id, attribution_correlation_id)
);

CREATE INDEX IF NOT EXISTS idx_run_outcomes_experiment
    ON run_outcomes (experiment_id);

ALTER TABLE prompts ADD COLUMN experiment_id BIGINT;
ALTER TABLE prompts ADD COLUMN experiment_variant TEXT;
ALTER TABLE cost_ledger ADD COLUMN experiment_id BIGINT;
ALTER TABLE cost_ledger ADD COLUMN experiment_variant TEXT;
ALTER TABLE request_failures ADD COLUMN experiment_id BIGINT;
ALTER TABLE request_failures ADD COLUMN experiment_variant TEXT;

ALTER TABLE cost_ledger ADD COLUMN tokens_estimated BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX IF NOT EXISTS idx_prompts_experiment
    ON prompts (experiment_id, created_at);
CREATE INDEX IF NOT EXISTS idx_cost_ledger_experiment
    ON cost_ledger (experiment_id, created_at);
CREATE INDEX IF NOT EXISTS idx_request_failures_experiment
    ON request_failures (experiment_id, created_at);

CREATE INDEX IF NOT EXISTS idx_prompts_correlation
    ON prompts (attribution_correlation_id, created_at);

-- Feedback looks a run up by (user, correlation id); correlation ids are
-- caller-chosen and may repeat across users, so the user leads the index.
CREATE INDEX IF NOT EXISTS idx_cost_ledger_user_correlation
    ON cost_ledger (user_id, attribution_correlation_id);
CREATE INDEX IF NOT EXISTS idx_request_failures_user_correlation
    ON request_failures (user_id, attribution_correlation_id);
