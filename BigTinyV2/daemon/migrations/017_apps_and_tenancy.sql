-- Per-app tenancy: the change that turns a single-owner daemon into a
-- multi-app one.
--
-- V1 had no client dimension at all -- no app, tenant, or owner column
-- anywhere. That was coherent while exactly one process (Kitty) spawned the
-- daemon, owned its lifetime, and treated every row in it as its own. It stops
-- being coherent the moment a second frontend attaches: `GET /api/chat/`
-- returned every session unconditionally, and Kitty's provider activation
-- PATCHed `fallback_priority: 100` onto every row it did not own.
--
-- Everything an app can create is therefore keyed by `app_id` here. Two
-- different nullability rules, deliberately:
--
--   * sessions/recipes/schedules/hitl_rules -- NOT NULL. These are always
--     owned by exactly one app; there is no meaningful "shared session".
--   * providers/mcp_servers -- nullable, where NULL means a shared pool
--     visible to every app. A user configuring one Anthropic key should not
--     have to re-enter it per app, so sharing has to be expressible.
--
-- Because V2 starts with no live client, there is no `DEFAULT 'kitty'`
-- back-fill and no seeded app row: every session has a real owner from the
-- first row written. SQLite requires *some* default on `ADD COLUMN NOT NULL`,
-- so '' is used as a placeholder that no code path may ever produce -- a row
-- carrying it is a scoping bug, and `routes_smoke.rs` asserts it stays
-- unreachable. Kitty's existing database is stamped with a real 'kitty' owner
-- by the Phase 7 import, not here.

CREATE TABLE IF NOT EXISTS apps (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    -- SHA-256 of the issued key. The key itself is shown once, at
    -- registration, and never stored -- so a leaked database cannot be
    -- replayed as a set of credentials.
    key_hash TEXT NOT NULL,
    scopes TEXT NOT NULL DEFAULT '["*"]',
    -- Replaces V1's global `fallback_priority` sort as the answer to "which
    -- provider does this caller get when it asks for none". That sort was
    -- daemon-wide, which is why Kitty had to demote everyone else's rows to
    -- express "use mine"; per-app defaults remove the need for that entirely.
    default_provider_id TEXT,
    default_model TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at TEXT
);

ALTER TABLE sessions      ADD COLUMN app_id TEXT NOT NULL DEFAULT '';
ALTER TABLE recipes       ADD COLUMN app_id TEXT NOT NULL DEFAULT '';
ALTER TABLE schedule_jobs ADD COLUMN app_id TEXT NOT NULL DEFAULT '';
ALTER TABLE hitl_rules    ADD COLUMN app_id TEXT NOT NULL DEFAULT '';

-- NULL = shared pool, visible to every app.
ALTER TABLE providers   ADD COLUMN app_id TEXT;
ALTER TABLE mcp_servers ADD COLUMN app_id TEXT;

-- Supersedes 007's idx_sessions_updated_at for the listing query, which is now
-- always filtered by app. The old index is left in place: it still serves the
-- retention sweep, which is deliberately daemon-wide.
CREATE INDEX IF NOT EXISTS idx_sessions_app_updated ON sessions(app_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_recipes_app ON recipes(app_id);
CREATE INDEX IF NOT EXISTS idx_schedule_jobs_app ON schedule_jobs(app_id);
CREATE INDEX IF NOT EXISTS idx_hitl_rules_app ON hitl_rules(app_id);
CREATE INDEX IF NOT EXISTS idx_providers_app ON providers(app_id);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_app ON mcp_servers(app_id);

-- `mcp_servers.name` had no uniqueness constraint in V1, and Kitty's
-- `ensure_builtin_servers` upserts by name from three call sites (boot, a ~2min
-- self-heal tick, and every Settings toggle). It guarded the race with a
-- process-local `static Mutex`, which cannot help once a second client is
-- doing the same thing -- both would create. COALESCE keeps shared rows
-- (app_id IS NULL) in the same namespace as each other while letting two apps
-- each own a server of the same name.
CREATE UNIQUE INDEX IF NOT EXISTS idx_mcp_app_name
    ON mcp_servers(COALESCE(app_id, ''), name);
