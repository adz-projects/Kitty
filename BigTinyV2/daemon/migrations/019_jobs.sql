-- Detached jobs: work that outlives the client that submitted it.
--
-- Every V1 route was chat-shaped -- create a session, POST /send, hold an SSE
-- stream. That is right for a chat window and wrong for a pipeline: if the
-- client goes away the turn is cancelled after `disconnect_grace_secs`, so a
-- batch job could not survive its submitter restarting.
--
-- The machinery already existed. `Agent::run_turn_and_wait` runs a turn with no
-- live client for recipes and the scheduler; it simply had no HTTP surface.
-- This table is that surface's durable half.
--
-- `parent_session_id` on `sessions` is the other half: concurrent turns within
-- one app are concurrent *sessions* (the per-session 409 is correct and stays),
-- so an app fanning out to subagents needs a way to group the children it
-- created. Deliberately just a tag -- orchestrating subagents belongs in the
-- app, not the daemon.

CREATE TABLE IF NOT EXISTS jobs (
    id         TEXT PRIMARY KEY,
    app_id     TEXT NOT NULL,
    -- The session the turn runs in. Nullable so a job whose session is later
    -- deleted becomes an orphaned audit row rather than vanishing with it.
    session_id TEXT REFERENCES sessions(id) ON DELETE SET NULL,
    prompt     TEXT NOT NULL,
    -- pending  : queued, not yet started
    -- running  : a turn is in flight
    -- succeeded / failed : terminal
    -- cancelled: withdrawn by its owner
    -- interrupted: the daemon stopped while this was running. Deliberately
    --   NOT re-queued: a turn may have had side effects (tool calls that wrote
    --   files, sent requests), and silently re-running it would repeat them.
    --   The owner decides whether to resubmit.
    status     TEXT NOT NULL DEFAULT 'pending'
               CHECK(status IN ('pending','running','succeeded','failed','cancelled','interrupted')),
    result     TEXT,
    error      TEXT,
    -- JSON: provider/model pins, response schema, cache directives.
    options    TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    started_at TEXT,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_jobs_app_created ON jobs(app_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);

-- Groups the children an app spawned for a fan-out. A tag, not a foreign key
-- with cascade: deleting a parent must not silently delete the subagent
-- transcripts, which are often the actual output.
ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
