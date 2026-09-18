-- Per-session incognito/pause flag. Kitty exposes a single "pause memory for
-- this session" control in the chat header that pauses BOTH the behavioral
-- (adaptive-pathway) and factual (memorabilia) engines; this table is
-- memorabilia's half of it. When a session is paused the daemon skips both
-- the turn-start recall injection and the turn-end learn/ingest for that
-- session — byte-identical to no-memory behavior, and only for that session.
-- An absent row means "not paused". Mirrors adaptive-pathway's
-- `conversation_state.paused`, kept as its own tiny table here because
-- memorabilia otherwise has no per-session conversation state.
CREATE TABLE IF NOT EXISTS session_pause (
    session_id TEXT PRIMARY KEY,
    paused     INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);
