-- Per-app plugin selection.
--
-- A *plugin* is an extension that hooks the agent loop directly -- adaptive
-- pathway injects recall before the LLM call, learns after it, runs a
-- background sweep, and owns its own routes. That is a different thing from an
-- *MCP server*, which only provides tools across the MCP boundary, and the two
-- must not be conflated:
--
--   * MCP servers live in `mcp_servers` and are selected per app by the
--     `app_id` column migration 017 added. Nothing more is needed for them.
--   * Plugins hook the loop and carry per-app *instance state* -- a belief
--     graph is memory about how one app is used, and merging two apps' graphs
--     would be both a privacy leak and a quality regression, since the beliefs
--     would describe nobody in particular.
--
-- Hence a separate table rather than one shared "extensions" list. The axis is
-- loop integration, not statefulness: `kitty-tools` is stateful (scratchpad,
-- doc cache) and is still only an MCP server.
--
-- No rows are seeded. `PluginHost` treats an absent row as "use the daemon
-- default" (`BIGTINYV2_PATHWAY__ENABLED`, itself defaulting to the config), so
-- an app that never expresses a preference behaves exactly as V1 did.

CREATE TABLE IF NOT EXISTS app_plugins (
    app_id  TEXT NOT NULL,
    -- Currently only 'pathway'. Deliberately a free-text column rather than a
    -- CHECK constraint: adding a plugin should not require a migration.
    plugin  TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    -- Plugin-specific JSON. Encrypt via `crypto::encrypt` before writing here
    -- if a plugin ever needs a credential; nothing does today.
    config  TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (app_id, plugin)
);
