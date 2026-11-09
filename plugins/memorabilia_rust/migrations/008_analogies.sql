-- Phase 4.7: analogy edges for indirect relevance.
--
-- One abstraction per claim: the general principle it is an instance of, with
-- its names, places and dates removed, embedded so claims from different
-- fields that share a shape sit close together.
CREATE TABLE IF NOT EXISTS claim_abstracts (
    node_id         TEXT PRIMARY KEY REFERENCES propositions(node_id) ON DELETE CASCADE,
    abstract        TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    embedding       BLOB NOT NULL
);

-- Why a confirmed RHYMES_WITH edge holds (one line), shown in recall next to
-- the analogous item.
CREATE TABLE IF NOT EXISTS edge_rationales (
    edge_id   TEXT PRIMARY KEY REFERENCES proposition_edges(edge_id) ON DELETE CASCADE,
    rationale TEXT NOT NULL
);
