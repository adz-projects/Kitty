-- Specialists: named delegate agents the main model can call mid-turn.
--
-- This replaces `recipes` rather than sitting beside it. A recipe was a
-- parameterized prompt template (minijinja, typed `parameters`) that a *user*
-- invoked by name -- a syntax to learn before it did anything. A specialist is
-- selected by the model from the user's ordinary request, so the user learns
-- nothing, and the safety that `parameters` used to provide by declaring a
-- shape moves to `tool_allow` and `response_schema`, which are enforced rather
-- than declared (see `agent::loop_`'s dispatch check and
-- `provider::schema::validate`).
--
-- `app_id` is NULLABLE here, unlike `recipes`, which was NOT NULL:
--
--   * NULL  -- a built-in, visible to every app, seeded by
--             `specialists::registry::seed_builtins`. Modifying or deleting one
--             is a 403, for the same reason a shared `mcp_servers` row is: it
--             is what *every* app's agent loop would run.
--   * set   -- an app's own specialist. If it shares a `name` with a built-in
--             it shadows it for that app, which is how "editing a built-in"
--             works without letting one app rewrite another's. Deleting the
--             shadow reverts to the built-in, matching `/api/apps/me/plugins`.

CREATE TABLE IF NOT EXISTS specialists (
    id          TEXT PRIMARY KEY,
    -- NULL = built-in, shared by every app. See above.
    app_id      TEXT,
    -- The routing key the model passes to `call_specialist`.
    name        TEXT NOT NULL,
    -- The only text the model sees when deciding whether to delegate. A vague
    -- one does not fail loudly; it just gets called for the wrong things.
    description TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    -- Optional pins, resolved through the ordinary per-app provider order
    -- (explicit pin > app default > healthiest visible).
    provider    TEXT,
    model       TEXT,
    -- JSON array of exact tool names. An empty array means a specialist with
    -- no tools, which is a valid (if unusual) definition -- it is NOT read as
    -- "unrestricted", since that would give the most restricted definition the
    -- widest surface.
    tool_allow  TEXT NOT NULL DEFAULT '[]',
    -- JSON Schema the final answer must validate against. NULL means prose,
    -- which is allowed but forfeits the guarantee the caller usually wants.
    response_schema TEXT,
    max_steps   INTEGER NOT NULL DEFAULT 20,
    -- Optional per-specialist ceiling, applied on top of the daemon-wide
    -- `agent.max_concurrent_specialists`. Only ever lowers it.
    max_concurrent INTEGER,
    enabled     INTEGER NOT NULL DEFAULT 1,
    builtin     INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

-- One definition per name per owner. `COALESCE` because SQLite treats every
-- NULL as distinct in a UNIQUE index, which would otherwise allow any number
-- of built-ins called "researcher".
CREATE UNIQUE INDEX IF NOT EXISTS idx_specialists_owner_name
    ON specialists(COALESCE(app_id, ''), name);
CREATE INDEX IF NOT EXISTS idx_specialists_app ON specialists(app_id);

-- `schedule_jobs` stops pointing at a recipe.
--
-- A cron-fired run is now an ordinary turn with a plain prompt, whose model may
-- itself call a specialist -- so the scheduler no longer needs a concept of a
-- pre-rendered task at all. Rebuilt rather than ALTERed because the old column
-- carries a foreign key into the table being dropped below, and SQLite cannot
-- drop a column that a constraint references.
--
-- Existing rows carry their recipe's prompt template forward as the prompt,
-- which is the closest honest translation: an un-parameterized template *is* a
-- prompt, and a parameterized one at least names what the job was for. A row
-- whose recipe is already gone gets its own name, so a schedule the user can
-- see and fix survives instead of vanishing.
CREATE TABLE schedule_jobs_new (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    cron       TEXT NOT NULL,
    prompt     TEXT NOT NULL,
    enabled    INTEGER DEFAULT 1,
    app_id     TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO schedule_jobs_new (id, name, cron, prompt, enabled, app_id, created_at, updated_at)
SELECT s.id,
       s.name,
       s.cron,
       COALESCE(r.prompt_template, s.name),
       s.enabled,
       s.app_id,
       s.created_at,
       s.updated_at
FROM schedule_jobs s
LEFT JOIN recipes r ON r.id = s.recipe_id;

DROP TABLE schedule_jobs;
ALTER TABLE schedule_jobs_new RENAME TO schedule_jobs;

-- Dropped with the old table; recreate it against the new one.
CREATE INDEX IF NOT EXISTS idx_schedule_jobs_app ON schedule_jobs(app_id);

DROP TABLE IF EXISTS recipes;
