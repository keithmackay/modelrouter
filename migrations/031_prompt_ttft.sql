-- Time to first token, recorded per request on the prompt row alongside
-- latency_ms. For a non-streamed provider call this is the time from
-- dispatching the provider request to receiving the response headers (the
-- elapsed time of the HTTP send before the body is read); for a streamed
-- response it is the time to the first body chunk. NULL where it was not
-- measured: rows written before this migration, cache hits, and providers
-- whose SDK exposes no header/body split.
ALTER TABLE prompts ADD COLUMN ttft_ms INTEGER;
