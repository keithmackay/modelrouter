-- migrations/postgres/004_mcp_servers.sql
CREATE TABLE IF NOT EXISTS mcp_servers (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    url         TEXT NOT NULL,
    description TEXT,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TEXT NOT NULL DEFAULT (to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'))
);

CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled ON mcp_servers(enabled);
