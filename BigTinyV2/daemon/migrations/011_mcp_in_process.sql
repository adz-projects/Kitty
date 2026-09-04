-- Add 'in_process' to the mcp_servers transport CHECK constraint.
-- Required for the pathway MCP server which uses an in-memory duplex pipe
-- instead of spawning a child process or reaching over HTTP.
CREATE TABLE mcp_servers_new (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    transport TEXT NOT NULL CHECK(transport IN ('stdio', 'sse', 'streamable_http', 'in_process')),
    command TEXT,
    args TEXT,
    url TEXT,
    env TEXT,
    headers TEXT,
    enabled INTEGER DEFAULT 1,
    status TEXT DEFAULT 'disconnected' CHECK(status IN ('connected', 'disconnected', 'error')),
    error_message TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO mcp_servers_new
    (id, name, transport, command, args, url, env, headers, enabled, status, error_message, created_at, updated_at)
SELECT id, name, transport, command, args, url, env, headers, enabled, status, error_message, created_at, updated_at
FROM mcp_servers;
DROP TABLE mcp_servers;
ALTER TABLE mcp_servers_new RENAME TO mcp_servers;
