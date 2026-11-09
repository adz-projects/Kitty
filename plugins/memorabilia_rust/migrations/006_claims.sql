-- Phase 4.2-4.4 of the memory revision plan: claims that know who asserted
-- them, merge with their paraphrases, and can be disputed or superseded one
-- claim at a time.

-- Who asserts the claim (empty when the source states it as its own fact)
-- and how: `states` (as fact), `reports` (relays someone else's claim),
-- `argues` (an argument or opinion), `refutes` (denies another claim).
ALTER TABLE propositions ADD COLUMN attribution TEXT NOT NULL DEFAULT '';
ALTER TABLE propositions ADD COLUMN stance TEXT NOT NULL DEFAULT 'states'
    CHECK (stance IN ('states', 'reports', 'argues', 'refutes'));

-- One embedding per claim, so a new claim that paraphrases an existing one
-- (confirmed by the extraction model) supports it instead of becoming a
-- second, single-source proposition.
CREATE TABLE IF NOT EXISTS claim_vectors (
    node_id         TEXT PRIMARY KEY REFERENCES propositions(node_id) ON DELETE CASCADE,
    embedding_model TEXT NOT NULL,
    embedding       BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_claim_vectors_model ON claim_vectors(embedding_model);

-- Claim-level relations. `disputes`: from_node contradicts to_node (both are
-- shown, both lose weight by `strength`). `supersedes`: from_node is the
-- newer state of affairs that replaces to_node ("was closed" -> "reopened");
-- the superseded claim leaves recall but stays readable. Replaces the
-- chunk-level `disputed_edges` for new extractions, which weakened every
-- claim in both chunks.
CREATE TABLE IF NOT EXISTS proposition_relations (
    from_node TEXT NOT NULL REFERENCES propositions(node_id) ON DELETE CASCADE,
    to_node   TEXT NOT NULL REFERENCES propositions(node_id) ON DELETE CASCADE,
    kind      TEXT NOT NULL CHECK (kind IN ('disputes', 'supersedes')),
    strength  REAL NOT NULL CHECK (strength >= 0 AND strength <= 1),
    reason    TEXT NOT NULL DEFAULT '',
    opened_at TEXT NOT NULL,
    closed_at TEXT,
    CHECK (from_node != to_node),
    PRIMARY KEY (from_node, to_node, kind)
);
CREATE INDEX IF NOT EXISTS idx_prop_rel_to ON proposition_relations(to_node);
