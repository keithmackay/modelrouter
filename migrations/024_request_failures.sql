-- Every request that FAILED, captured.
--
-- `prompts` rows are written only on the success path, so until now a request
-- that never reached a provider — or that a provider rejected — left no trace
-- in the router at all. The operator's only evidence was the calling app's own
-- logs, which is exactly backwards: the router is the one component that sees
-- every call from every app.
--
-- Concretely (Athena, 2026-08-09): 196 requests failed with
-- "Unknown provider: anthropic" and the router's own history showed nothing,
-- because a provider-resolution failure produces no prompt row. Diagnosis
-- required reading the caller's worker log and reverse-engineering the routing
-- from it.
--
-- `stage` names WHERE the request died, which is what makes a failure
-- actionable without a stack trace:
--   'resolve'  — model/provider resolution (unknown provider, no adapter)
--   'policy'   — budget, rate limit, guardrail or policy denial
--   'provider' — the upstream was reached and returned an error
--   'request'  — the caller's request was malformed or unacceptable
--   'internal' — anything else the router itself raised
--
-- No prompt/response bodies are stored: a failure record must never become a
-- second, unlogged copy of prompt content that `X-No-Log: true` suppressed.
CREATE TABLE IF NOT EXISTS request_failures (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id       INTEGER REFERENCES users(id),
    api_key_id    INTEGER REFERENCES api_keys(id),
    endpoint      TEXT NOT NULL,
    request_model TEXT NOT NULL,
    routed_model  TEXT,
    provider      TEXT,
    stage         TEXT NOT NULL,
    status_code   INTEGER,
    error_message TEXT NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 1,
    latency_ms    INTEGER,
    project       TEXT,
    attribution_correlation_id TEXT,
    attribution_tags TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_request_failures_created
    ON request_failures (created_at);

CREATE INDEX IF NOT EXISTS idx_request_failures_stage
    ON request_failures (stage, created_at);

CREATE INDEX IF NOT EXISTS idx_request_failures_correlation
    ON request_failures (attribution_correlation_id, created_at);
