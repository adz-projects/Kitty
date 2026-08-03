CREATE TABLE mcp_servers_new (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    transport TEXT NOT NULL CHECK(transport IN ('stdio', 'sse', 'streamable_http')),
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
    (id, name, transport, command, args, url, env, enabled, status, error_message, created_at, updated_at)
SELECT id, name, transport, command, args, sse_url, env, enabled, status, error_message, created_at, updated_at
FROM mcp_servers;
DROP TABLE mcp_servers;
ALTER TABLE mcp_servers_new RENAME TO mcp_servers;
