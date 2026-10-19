-- Per-alias parameters for scoped alias overrides: a JSON object of request
-- parameters (e.g. reasoning_effort, max_tokens, temperature) validated
-- against the pinned model's parameter schema when written, and applied to
-- chat completion requests the override routes, overwriting the caller's.
ALTER TABLE scoped_aliases ADD COLUMN params TEXT NOT NULL DEFAULT '{}';
