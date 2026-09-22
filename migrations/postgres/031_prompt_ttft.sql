-- Time to first token per request. See migrations/031_prompt_ttft.sql for the
-- rationale; this is the PostgreSQL spelling of the same change.
ALTER TABLE prompts ADD COLUMN ttft_ms BIGINT;
