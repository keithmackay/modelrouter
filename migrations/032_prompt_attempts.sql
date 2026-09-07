-- Provider attempts per request, recorded on the prompt row: how many
-- provider calls (first try + retries + failover hops that reached a
-- provider) the request took before the recorded response. 1 means
-- first-try success. Distinct from the request_failures table, which records
-- requests that never succeeded at all. NULL where it was not tracked: rows
-- written before this migration, and cache hits (no provider call at all).
ALTER TABLE prompts ADD COLUMN attempts INTEGER;
