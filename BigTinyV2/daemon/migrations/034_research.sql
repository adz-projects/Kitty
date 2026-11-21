-- Deep research runs and their ledger.
--
-- A research run turns a brief into a sourced deliverable over a budget of
-- turns: turn 0 drafts a contract the user approves, turns 1..N-2 are waves
-- (a planner on a fresh context picks a frontier, delegates research it in
-- parallel), turn N-1 is an adversarial critique and turn N writes and
-- verifies. The only state carried from step to step is the ledger below:
-- never a transcript, never a prose summary that each step rewrites.
--
-- The ledger is append-only. Entries are facts about what was found, asked,
-- suggested or ruled out; what became of them (answered, pursued, a duplicate,
-- disputed, superseded) is recorded as links and derived on read. Triggers
-- refuse UPDATE so the history of a run cannot be rewritten after the fact.

CREATE TABLE IF NOT EXISTS research_runs (
    id          TEXT PRIMARY KEY,
    app_id      TEXT NOT NULL,
    -- The chat the run belongs to (turn 0 and the final report appear there).
    session_id  TEXT REFERENCES sessions(id) ON DELETE SET NULL,
    brief       TEXT NOT NULL,
    -- planning   : turn 0, drafting the contract and waiting for approval
    -- researching: waves
    -- critiquing : the adversarial turn
    -- writing    : the final write-and-verify turn
    -- done / failed / cancelled: terminal
    status      TEXT NOT NULL DEFAULT 'planning'
                CHECK(status IN ('planning','researching','critiquing','writing',
                                 'done','failed','cancelled')),
    max_turns   INTEGER NOT NULL CHECK(max_turns >= 3),
    -- Counted turns finished (turn 0 is not counted).
    turns_used  INTEGER NOT NULL DEFAULT 0,
    -- JSON: the approved contract, and the writer's outline once fixed.
    contract    TEXT,
    outline     TEXT,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    output_path TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_research_runs_app ON research_runs(app_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_research_runs_status ON research_runs(status);
CREATE INDEX IF NOT EXISTS idx_research_runs_session ON research_runs(session_id);

-- Ledger entries. `seq` is the run-local id the models cite as `L<seq>`.
-- `norm_key` makes an exact repeat (the same URL, the same normalized claim)
-- land on the existing row instead of a second one; NULL keys never collide.
CREATE TABLE IF NOT EXISTS research_entries (
    run_id     TEXT NOT NULL REFERENCES research_runs(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    wave       INTEGER NOT NULL,
    kind       TEXT NOT NULL
               CHECK(kind IN ('finding','source','question','lead','dead_end',
                              'entity','defect','task')),
    payload    TEXT NOT NULL,
    norm_key   TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (run_id, seq),
    UNIQUE (run_id, kind, norm_key)
);

CREATE INDEX IF NOT EXISTS idx_research_entries_kind ON research_entries(run_id, kind);

-- Relations between entries. Direction reads "from <kind> to":
--   cites        finding -> source it rests on
--   answers      finding -> question or lead it answers
--   duplicates   entry   -> the entry it repeats (the target is kept)
--   disputes     finding -> finding it contradicts (both stay visible)
--   supersedes   finding -> older finding it replaces
--   derived_from lead    -> finding/source it was read out of
--   pursues      task    -> lead or question it acts on
--   addresses    finding -> defect it resolves
CREATE TABLE IF NOT EXISTS research_links (
    run_id     TEXT NOT NULL REFERENCES research_runs(id) ON DELETE CASCADE,
    from_seq   INTEGER NOT NULL,
    to_seq     INTEGER NOT NULL,
    kind       TEXT NOT NULL
               CHECK(kind IN ('cites','answers','duplicates','disputes','supersedes',
                              'derived_from','pursues','addresses')),
    reason     TEXT NOT NULL DEFAULT '',
    wave       INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (from_seq != to_seq),
    PRIMARY KEY (run_id, from_seq, to_seq, kind)
);

CREATE INDEX IF NOT EXISTS idx_research_links_to ON research_links(run_id, to_seq);

-- Placement of an entry against the contract: which coverage cell it counts
-- toward and which section of the deliverable it belongs in. Targets are the
-- contract's own ids, not entries, so they live apart from links.
CREATE TABLE IF NOT EXISTS research_tags (
    run_id   TEXT NOT NULL REFERENCES research_runs(id) ON DELETE CASCADE,
    seq      INTEGER NOT NULL,
    tag_kind TEXT NOT NULL CHECK(tag_kind IN ('cell','section')),
    tag      TEXT NOT NULL,
    wave     INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq, tag_kind, tag)
);

-- One row per step the driver ran (turn 0, each wave, the critic, the
-- writer), with the child session that did the work so a client can open it.
CREATE TABLE IF NOT EXISTS research_steps (
    run_id      TEXT NOT NULL REFERENCES research_runs(id) ON DELETE CASCADE,
    turn        INTEGER NOT NULL,
    phase       TEXT NOT NULL CHECK(phase IN ('plan','wave','critic','write')),
    session_id  TEXT,
    status      TEXT NOT NULL DEFAULT 'running'
                CHECK(status IN ('running','succeeded','failed','skipped')),
    summary     TEXT,
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT,
    PRIMARY KEY (run_id, turn)
);

CREATE TRIGGER IF NOT EXISTS research_entries_append_only
BEFORE UPDATE ON research_entries
BEGIN
    SELECT RAISE(ABORT, 'research ledger entries are append-only');
END;

CREATE TRIGGER IF NOT EXISTS research_links_append_only
BEFORE UPDATE ON research_links
BEGIN
    SELECT RAISE(ABORT, 'research ledger links are append-only');
END;
