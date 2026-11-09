-- Phase 3 of the memory revision plan.
--
-- What sort of belief this is: `preference` (how the user likes things done),
-- `stance` (a view the user holds about the world -- what [Worth challenging]
-- offers counterpoints to), `context` (what they are working on, their
-- situation) or `trait` (who they are). Existing rows default to `context`.
ALTER TABLE beliefs ADD COLUMN kind TEXT NOT NULL DEFAULT 'context';

-- Typed connections between beliefs, from extraction (`relates_to`): A
-- explains B, A is in tension with B, A is a special case of B. Diffused
-- alongside cosine and co-occurrence edges at recall; `in_tension_with` also
-- opens a contradiction so [In tension] can surface it.
CREATE TABLE IF NOT EXISTS belief_links (
    belief_a   TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    belief_b   TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    relation   TEXT NOT NULL CHECK (relation IN ('explains', 'in_tension_with', 'specializes')),
    created_at TEXT NOT NULL,
    PRIMARY KEY (belief_a, belief_b, relation)
);
CREATE INDEX IF NOT EXISTS idx_belief_links_b ON belief_links(belief_b);

-- [Worth testing] shown at most once per chat: which assumption was put to
-- which chat, on which message (a regenerate of that same message shows it
-- again; any other message does not). Also the "tests in flight" the next
-- extraction pass is asked to judge.
CREATE TABLE IF NOT EXISTS assumption_tests (
    assumption_id TEXT NOT NULL,
    session_id    TEXT NOT NULL,
    message_hash  TEXT NOT NULL,
    shown_at      TEXT NOT NULL,
    judged        INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (assumption_id, session_id)
);

-- `tested` now means validated (a correction or a controlled/assumption test).
-- A direct statement keeps its full weight through its provenance
-- (`Belief::full_weight`), not through this flag.
UPDATE beliefs SET tested = 0 WHERE provenance = 'direct_statement';
