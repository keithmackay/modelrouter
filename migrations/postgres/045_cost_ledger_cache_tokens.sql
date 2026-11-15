-- Prompt-cache token counts on the cost ledger. Until now they lived only on
-- `prompts`, which the storage policy may skip, so a ledger row could not say
-- what share of its input was served from the provider's prompt cache. Zero
-- where the request path does not know them.
ALTER TABLE cost_ledger ADD COLUMN IF NOT EXISTS cache_read_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE cost_ledger ADD COLUMN IF NOT EXISTS cache_write_tokens BIGINT NOT NULL DEFAULT 0;
