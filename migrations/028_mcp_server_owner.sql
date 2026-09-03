-- migrations/028_mcp_server_owner.sql
-- Record who registered an MCP server so writes can be scoped to their owner.
--
-- Before this, /v1/mcp/servers POST/PATCH/DELETE authenticated the caller and
-- discarded the identity, so any valid API key could edit or delete any
-- server. Existing rows get NULL, which the handlers treat as unowned and
-- refuse to mutate through the key-authenticated API.
ALTER TABLE mcp_servers ADD COLUMN owner_user_id INTEGER REFERENCES users(id);

CREATE INDEX IF NOT EXISTS idx_mcp_servers_owner ON mcp_servers(owner_user_id);
