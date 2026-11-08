-- Selective intake, learned overnight (docs/MEMORY_OVERNIGHT.md).
--
-- `captures`: what a turn brought in, after the paragraph screen. While a
-- capture waits for the decision model it keeps its raw text (`raw_text`,
-- status `unscreened`); once screened it keeps only the kept paragraphs
-- (`kept_paragraphs`, a JSON array of strings). `document_hash` is the
-- whole-payload SHA-256, the same key `documents` uses, so a document is
-- captured once.
CREATE TABLE IF NOT EXISTS captures (
    capture_id       TEXT PRIMARY KEY,
    document_hash    TEXT NOT NULL UNIQUE,
    session_id       TEXT,
    turn_id          INTEGER,
    source_type      TEXT NOT NULL,
    source_name      TEXT NOT NULL,
    source_entity    TEXT NOT NULL,
    intent_factor    REAL NOT NULL,
    captured_at      TEXT NOT NULL,
    status           TEXT NOT NULL
        CHECK (status IN ('unscreened', 'screened', 'dropped', 'processed')),
    raw_text         TEXT,
    kept_paragraphs  TEXT,
    paragraphs_total INTEGER NOT NULL DEFAULT 0,
    paragraphs_kept  INTEGER NOT NULL DEFAULT 0,
    pinned           INTEGER NOT NULL DEFAULT 0,
    screened_at      TEXT
);
CREATE INDEX IF NOT EXISTS idx_captures_status ON captures (status, captured_at);

-- Search queries the chat model issued, per turn: the demand signal that
-- lowers a topic's promotion bar when the user keeps coming back to it.
CREATE TABLE IF NOT EXISTS demand_queries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    turn_id         INTEGER NOT NULL,
    query           TEXT NOT NULL,
    embedding       BLOB,
    embedding_model TEXT,
    created_at      TEXT NOT NULL,
    UNIQUE (session_id, turn_id, query)
);
CREATE INDEX IF NOT EXISTS idx_demand_queries_turn ON demand_queries (session_id, turn_id);

-- Chunks waiting for their topic to recur. Never recalled: they live only
-- here, outside `chunks` and `chunks_vec`, until promoted.
CREATE TABLE IF NOT EXISTS candidates (
    candidate_id    TEXT PRIMARY KEY,
    capture_id      TEXT NOT NULL,
    session_id      TEXT,
    turn_id         INTEGER,
    content         TEXT NOT NULL,
    content_hash    TEXT NOT NULL UNIQUE,
    embedding       BLOB NOT NULL,
    embedding_model TEXT NOT NULL,
    pinned          INTEGER NOT NULL DEFAULT 0,
    status          TEXT NOT NULL
        CHECK (status IN ('waiting', 'promoted', 'merged')),
    created_at      TEXT NOT NULL,
    decided_at      TEXT
);
CREATE INDEX IF NOT EXISTS idx_candidates_status ON candidates (status, created_at);

-- Every paragraph the intake judged, for labelling and for testing other
-- decision-model question sets. `verdict` is `kept`, `dropped`, or
-- `prefilter:<reason>`; `scores` is a JSON object of P(yes) by question name.
CREATE TABLE IF NOT EXISTS screen_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    capture_id   TEXT NOT NULL,
    source_type  TEXT NOT NULL,
    source_name  TEXT NOT NULL,
    heading      TEXT,
    text         TEXT NOT NULL,
    question_set TEXT,
    scores       TEXT,
    verdict      TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_screen_log_created ON screen_log (created_at);
