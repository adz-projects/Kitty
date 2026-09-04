-- Per-server tool-call timeout, in seconds. NULL = use the daemon default
-- (`mcp::manager::DEFAULT_TOOL_TIMEOUT`, 30s).
--
-- A single hardcoded 30s cut legitimately long tools (a big build, a slow
-- remote query) with no way to raise it, and forced short-running servers to
-- wait the full 30s before a hung call was declared dead.
ALTER TABLE mcp_servers ADD COLUMN timeout_s INTEGER;
