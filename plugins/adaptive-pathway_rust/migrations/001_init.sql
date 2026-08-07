-- Adaptive Pathway behavioral-memory schema (fresh `pathway.db`, own
-- migration chain -- rollback is a config flip, delete-everything is a file
-- delete). PRAGMAs (WAL / synchronous=NORMAL / foreign_keys=ON /
-- busy_timeout) are applied in code at pool open, mirroring the daemon.

-- Beliefs: the model's learned beliefs about the user. `layer` is
-- conversation | context | identity. The extractor may write only
-- conversation/context (identity is a schema-level guard -- only
-- consolidation promotes), enforced by the CHECK constraint.
CREATE TABLE IF NOT EXISTS beliefs (
    id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    embedding BLOB NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.0,
    -- correction | direct_statement | controlled_test | inferred_pattern | single_observation
    provenance TEXT NOT NULL DEFAULT 'single_observation',
    layer TEXT NOT NULL CHECK (layer IN ('identity', 'context', 'conversation')),
    tested INTEGER NOT NULL DEFAULT 0,
    domain TEXT,
    tier TEXT NOT NULL DEFAULT 'conversation',
    support_count INTEGER NOT NULL DEFAULT 1,
    distinct_sessions INTEGER NOT NULL DEFAULT 1,
    contradict_count INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    last_confirmed_at TIMESTAMP,
    consolidated_at TIMESTAMP,
    open_topics TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Assumptions: beliefs below the tested/surfacing boundary that the engine
-- schedules for testing. State machine: scheduled -> surfaced -> passed|failed.
CREATE TABLE IF NOT EXISTS assumptions (
    id TEXT PRIMARY KEY,
    belief_id TEXT REFERENCES beliefs(id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.0,
    state TEXT NOT NULL DEFAULT 'scheduled'
        CHECK (state IN ('scheduled', 'surfaced', 'passed', 'failed', 'stale')),
    exchanged_since_flag INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Contradictions: pairs of beliefs that conflict. Preserved, never silently
-- resolved. `resolved_b` records which belief the resolution sided with.
CREATE TABLE IF NOT EXISTS contradictions (
    id TEXT PRIMARY KEY,
    belief_a TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    belief_b TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved')),
    resolved_b TEXT,
    reason TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    resolved_at TIMESTAMP
);

-- Observations: raw per-turn extractions before merge/consolidation; kept as
-- an audit trail even after a belief row is re-parented or pruned.
CREATE TABLE IF NOT EXISTS observations (
    id TEXT PRIMARY KEY,
    belief_id TEXT REFERENCES beliefs(id) ON DELETE SET NULL,
    session_id TEXT,
    statement TEXT NOT NULL,
    provenance TEXT NOT NULL DEFAULT 'single_observation',
    layer TEXT NOT NULL CHECK (layer IN ('identity', 'context', 'conversation')),
    domain TEXT,
    evidence TEXT,
    contradicts TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Domains: named topic domains beliefs route through (cross-domain is a
-- routing decision, not deletion).
CREATE TABLE IF NOT EXISTS domains (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    centroid BLOB,
    dpp_diversity_weight REAL NOT NULL DEFAULT 1.0,
    novelty_lambda REAL NOT NULL DEFAULT 0.5,
    sessions INTEGER NOT NULL DEFAULT 0,
    belief_count INTEGER NOT NULL DEFAULT 0,
    last_inferred TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Suppressions: forget()ed beliefs and the 'don't retry' mutes. `permanent`
-- rows never expire; `expires_at`-backed rows are temporal.
CREATE TABLE IF NOT EXISTS suppressions (
    id TEXT PRIMARY KEY,
    belief_id TEXT,
    text_hash TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('wrong', 'outdated', 'private', 'duplicate')),
    permanent INTEGER NOT NULL DEFAULT 0,
    expires_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Per-session learning/pause state and the learn watermark.
CREATE TABLE IF NOT EXISTS conversation_state (
    session_id TEXT PRIMARY KEY,
    paused INTEGER NOT NULL DEFAULT 0,
    exchange_count INTEGER NOT NULL DEFAULT 0,
    last_learned_rowid INTEGER NOT NULL DEFAULT 0,
    last_recall_ids TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Forget tombstones: text-hash keys of permanently-forgotten beliefs so
-- extraction can never relearn them.
CREATE TABLE IF NOT EXISTS forget_tombstones (
    text_hash TEXT PRIMARY KEY,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Audit log: record/forget/pause/consolidate events for the Settings UI.
CREATE TABLE IF NOT EXISTS audit_log (
    id TEXT PRIMARY KEY,
    event TEXT NOT NULL,
    detail TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Persisted novelty CMS tables (visit counts per table/bucket).
CREATE TABLE IF NOT EXISTS novelty_tables (
    id TEXT PRIMARY KEY,
    table_index INTEGER NOT NULL,
    hash_bucket INTEGER NOT NULL,
    visit_count INTEGER NOT NULL DEFAULT 0,
    last_updated TIMESTAMP,
    UNIQUE (table_index, hash_bucket)
);

-- Synthesis log: what consolidation merged/promoted, for debugging.
CREATE TABLE IF NOT EXISTS synthesis_log (
    id TEXT PRIMARY KEY,
    session_id TEXT,
    summary TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Small settings KV store.
CREATE TABLE IF NOT EXISTS app_settings (
    key TEXT PRIMARY KEY,
    value TEXT
);

-- Core lookup indexes.
CREATE INDEX IF NOT EXISTS idx_beliefs_layer ON beliefs(layer);
CREATE INDEX IF NOT EXISTS idx_beliefs_domain ON beliefs(domain);
CREATE INDEX IF NOT EXISTS idx_beliefs_tested ON beliefs(tested);
CREATE INDEX IF NOT EXISTS idx_observations_session ON observations(session_id);
CREATE INDEX IF NOT EXISTS idx_assumptions_state ON assumptions(state);
CREATE INDEX IF NOT EXISTS idx_suppressions_hash ON suppressions(text_hash);
