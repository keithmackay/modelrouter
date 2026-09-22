-- Provider attempts per request. See migrations/032_prompt_attempts.sql for
-- the rationale; this is the PostgreSQL spelling of the same change.
ALTER TABLE prompts ADD COLUMN attempts BIGINT;
