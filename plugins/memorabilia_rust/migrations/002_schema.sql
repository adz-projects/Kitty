-- 002_schema.sql — full Level 0 / Level 1 schema (Phase 2, plan §15).
--
-- Append, never edit 001. The `chunks_vec` vec0 virtual table is
-- deliberately NOT created here: its `float[<dim>]` column type encodes the
-- configured `embedding_dim` (plan §3.3), a value static migrations cannot
-- know — `Db::init_vectors` creates it with the configured dim. `app_settings`
-- already exists from 001.
--
-- Conventions (mirroring the behavioral-memory reference's 001):
--   * TEXT primary keys, snake_case, CHECK constraints for every closed
--     column (state machines, tier ladders, edge types).
--   * Timestamps are TIMESTAMP columns holding ISO-8601 UTC strings
--     ("…Z"), always supplied by the caller; no CURRENT_TIMESTAMP defaults
--     so test state is fully deterministic.
--   * `owner_id` is reserved on every row table (claude.md principle 10:
--     single-user today, no migration forced later). It is nullable and
--     unindexed — nothing reads it yet.
--   * The store layer holds row types + SQL primitives only; no business
--     logic lives in these definitions (plan §15 Phase 2 exit).

-- Stage 1 catalog: whole-payload SHA-256, abort-on-seen (plan §3.1).
-- Rejected payloads (Stage 0 PII) create no row — nothing is written
-- before acceptance (plan §12.1); rejections go to audit_log.
CREATE TABLE IF NOT EXISTS documents (
    owner_id TEXT,
    document_hash TEXT PRIMARY KEY,
    source_entity TEXT NOT NULL,
    source_type TEXT NOT NULL,
    source_name TEXT NOT NULL,
    captured_at TIMESTAMP NOT NULL,
    -- Attachment intent factor (evidence < 1.0 < … casual per plan §4.2),
    -- the seed multiplier for the document's source_reliability.
    intent_factor REAL NOT NULL DEFAULT 1.0 CHECK (intent_factor > 0 AND intent_factor <= 1.0),
    chunk_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_documents_source ON documents(source_entity);

-- Level 0 evidence chunks (plan §5.1). Decay class + t_anchor live here —
-- decay/lifecycle belongs to chunks, never propositions (claude.md
-- principle 1). The τ time constants are Config knobs, not row columns:
-- one source of truth per tunable (claude.md principle 8), and the §5.1
-- decay_profile τ fields echo those Config values.
CREATE TABLE IF NOT EXISTS chunks (
    owner_id TEXT,
    chunk_id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL UNIQUE,
    document_hash TEXT NOT NULL REFERENCES documents(document_hash) ON DELETE CASCADE,
    source_entity TEXT NOT NULL,
    source_reliability REAL NOT NULL CHECK (source_reliability >= 0 AND source_reliability <= 1),
    provenance_cluster_id TEXT NOT NULL,
    cluster_citation TEXT NOT NULL,
    -- "active" | "archived"; "disputed" is a DERIVED flag (open DISPUTED
    -- edge), never a status (plan §5.3). "deleted" is a hard delete — no
    -- row, no status value.
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived')),
    extraction_status TEXT NOT NULL DEFAULT 'pending' CHECK (extraction_status IN ('pending', 'done')),
    -- Model tag of the vector space that produced the chunk's embedding
    -- (plan §3.3): the pinned semantic model, or __lexical_hash__ for the
    -- fallback. Retrieval/clustering filter by this tag.
    embedding_model TEXT NOT NULL,
    decay_class TEXT NOT NULL CHECK (decay_class IN ('static', 'transient', 'deadline')),
    anchor_at TIMESTAMP NOT NULL,
    urgency_expires_at TIMESTAMP,
    reinforcement_count INTEGER NOT NULL DEFAULT 0,
    grounding_count INTEGER NOT NULL DEFAULT 0,
    archived_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL,
    -- A deadline chunk must carry its T_expire; other classes must not.
    CHECK (
        (decay_class = 'deadline' AND urgency_expires_at IS NOT NULL)
        OR (decay_class != 'deadline' AND urgency_expires_at IS NULL)
    ),
    -- archived_at is set exactly when (and only when) status is archived.
    CHECK (
        (status = 'active' AND archived_at IS NULL)
        OR (status = 'archived' AND archived_at IS NOT NULL)
    )
);
CREATE INDEX IF NOT EXISTS idx_chunks_document ON chunks(document_hash);
CREATE INDEX IF NOT EXISTS idx_chunks_status ON chunks(status);
CREATE INDEX IF NOT EXISTS idx_chunks_cluster ON chunks(provenance_cluster_id);
-- The Stage 4 drain scans pending chunks oldest-first (rowid order).
CREATE INDEX IF NOT EXISTS idx_chunks_pending ON chunks(extraction_status);

-- Level 1 proposition nodes (plan §5.2). `confidence` (derived §8) and
-- `is_disputed` (derived: any supporting chunk has an open DISPUTED edge,
-- §9.1) are stored denormalized so retrieval never recomputes across joins;
-- the Phase 5/6/8 writers recompute them in the same transaction as their
-- causes.
CREATE TABLE IF NOT EXISTS propositions (
    owner_id TEXT,
    node_id TEXT PRIMARY KEY,
    claim TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.0 CHECK (confidence >= 0 AND confidence <= 1),
    is_disputed INTEGER NOT NULL DEFAULT 0 CHECK (is_disputed IN (0, 1)),
    -- "active" | "UNSUPPORTED_ARCHIVE" (plan §5.3). Deletion is a hard
    -- delete; archived rows hard-delete after archive_retention_days.
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'UNSUPPORTED_ARCHIVE')),
    importance TEXT NOT NULL DEFAULT 'unknown' CHECK (importance IN ('high', 'medium', 'low', 'unknown')),
    urgency TEXT NOT NULL DEFAULT 'unknown' CHECK (urgency IN ('high', 'medium', 'low', 'unknown')),
    -- Earliest-deadline rule resolution over the chunk's T_expires (§7.2).
    urgency_expires_at TIMESTAMP,
    last_assessed_at TIMESTAMP,
    archived_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL,
    CHECK (
        (status = 'active' AND archived_at IS NULL)
        OR (status = 'UNSUPPORTED_ARCHIVE' AND archived_at IS NOT NULL)
    )
);
CREATE INDEX IF NOT EXISTS idx_props_status ON propositions(status);
CREATE INDEX IF NOT EXISTS idx_props_importance ON propositions(importance);

-- Chunk → proposition support links (plan §15 Phase 2). A chunk supports
-- several propositions; a proposition is supported by several chunks.
-- N_active for confidence math is COUNT(DISTINCT source_entity) over this
-- join filtered to active chunks (plan §8.2) — business logic, not here.
CREATE TABLE IF NOT EXISTS chunk_propositions (
    owner_id TEXT,
    chunk_id TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    node_id TEXT NOT NULL REFERENCES propositions(node_id) ON DELETE CASCADE,
    created_at TIMESTAMP NOT NULL,
    PRIMARY KEY (chunk_id, node_id)
);
CREATE INDEX IF NOT EXISTS idx_cp_node ON chunk_propositions(node_id);

-- Proposition-level edges (plan §9.2). `weight` is the Pass 2 propagation
-- weight (DEPENDS_ON 0.8 / CAUSES 0.6 / BLOCKS 0.4 / MODIFIES_DEADLINE 0.7 /
-- RHYMES_WITH 0.5); ≤ 0.8 keeps accumulated activation in [0,1] with the
-- hop decay (plan §10.1). `revalidate_at`/`ttl_days` carry the RHYMES_WITH
-- TTL (plan §9.3) and are NULL for the other types.
CREATE TABLE IF NOT EXISTS proposition_edges (
    owner_id TEXT,
    edge_id TEXT PRIMARY KEY,
    edge_type TEXT NOT NULL CHECK (edge_type IN
        ('DEPENDS_ON', 'CAUSES', 'BLOCKS', 'MODIFIES_DEADLINE', 'RHYMES_WITH')),
    from_node TEXT NOT NULL REFERENCES propositions(node_id) ON DELETE CASCADE,
    to_node TEXT NOT NULL REFERENCES propositions(node_id) ON DELETE CASCADE,
    weight REAL NOT NULL CHECK (weight > 0 AND weight <= 0.8),
    revalidate_at TIMESTAMP,
    ttl_days INTEGER,
    created_at TIMESTAMP NOT NULL,
    CHECK (from_node != to_node),
    UNIQUE (edge_type, from_node, to_node)
);
CREATE INDEX IF NOT EXISTS idx_pe_from ON proposition_edges(from_node);
CREATE INDEX IF NOT EXISTS idx_pe_to ON proposition_edges(to_node);
CREATE INDEX IF NOT EXISTS idx_pe_type ON proposition_edges(edge_type);

-- The single dispute mechanism (plan §9.1): a SYMMETRIC chunk↔chunk edge,
-- written once in canonical order. Opening the same pair twice is a no-op
-- at the schema level (UNIQUE + canonical-order CHECK). `closed_at` is
-- set when either endpoint archives — archived materials drop out of
-- dispute; `strength` derives both endpoints' dispute_strength.
CREATE TABLE IF NOT EXISTS disputed_edges (
    owner_id TEXT,
    edge_id TEXT PRIMARY KEY,
    chunk_a TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    chunk_b TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    strength REAL NOT NULL CHECK (strength > 0 AND strength <= 1),
    opened_at TIMESTAMP NOT NULL,
    closed_at TIMESTAMP,
    reason TEXT NOT NULL,
    CHECK (chunk_a < chunk_b),
    UNIQUE (chunk_a, chunk_b)
);
CREATE INDEX IF NOT EXISTS idx_disputed_b ON disputed_edges(chunk_b);
CREATE INDEX IF NOT EXISTS idx_disputed_open ON disputed_edges(closed_at);

-- Per-source reliability registry (plan §4.2): one row per source_entity.
-- `reliability` is seeded as clamp(tier_prior × intent_factor) at first
-- sight and drifted by maintenance within ±reliability_band of
-- `tier_prior` — never hand-coded (claude.md principle 7). `tier_prior`
-- snapshots the data-file value at seed time so drift math stays stable if
-- data/reliability.yaml is later revised.
CREATE TABLE IF NOT EXISTS source_registry (
    owner_id TEXT,
    source_entity TEXT PRIMARY KEY,
    tier TEXT NOT NULL CHECK (tier IN ('primary', 'established', 'community', 'personal')),
    tier_prior REAL NOT NULL CHECK (tier_prior >= 0 AND tier_prior <= 1),
    reliability REAL NOT NULL CHECK (reliability >= 0 AND reliability <= 1),
    last_drift_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL,
    updated_at TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_registry_tier ON source_registry(tier);

-- Durable async reinforcement outbox (plan §6.2). Rows are written in the
-- same transaction as grounding; at-least-once delivery, and
-- UNIQUE(chunk_id, query_event_id) makes redelivery a no-op. Ordering is
-- FIFO on rowid; the drain worker is serialized behind the maintenance
-- lock.
CREATE TABLE IF NOT EXISTS reinforcement_outbox (
    owner_id TEXT,
    chunk_id TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    query_event_id TEXT NOT NULL,
    grounded INTEGER NOT NULL DEFAULT 1 CHECK (grounded IN (0, 1)),
    -- reinforced = grounded AND no open DISPUTED edge, resolved at write
    -- time (§6.1): a disputed chunk is grounded for audit, never reinforced.
    reinforced INTEGER NOT NULL DEFAULT 0 CHECK (reinforced IN (0, 1)),
    created_at TIMESTAMP NOT NULL,
    UNIQUE (chunk_id, query_event_id)
);

-- Forget tombstones (plan §12.2): text-hash keys that must never be
-- relearned. Matched at ingestion (content_hash / document_hash) and at
-- extraction (normalized proposition text). `private` tombstones are
-- permanent; `wrong` rows are too. The `outdated` reason is a suppression,
-- not a tombstone (see suppressions).
CREATE TABLE IF NOT EXISTS tombstones (
    owner_id TEXT,
    text_hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('content', 'document', 'proposition')),
    permanent INTEGER NOT NULL DEFAULT 1 CHECK (permanent IN (0, 1)),
    created_at TIMESTAMP NOT NULL
);

-- Forget-ladder suppressions (plan §12.2, Phase 3): chunks the user marked
-- `wrong` (permanent) or `outdated` (time-bounded) — excluded from recall
-- but not hard-deleted, unlike `private` (which cascades + tombstones).
-- Mirrors the reference's `suppressions` reason ladder.
CREATE TABLE IF NOT EXISTS suppressions (
    owner_id TEXT,
    chunk_id TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
    reason TEXT NOT NULL CHECK (reason IN ('wrong', 'outdated')),
    permanent INTEGER NOT NULL DEFAULT 0 CHECK (permanent IN (0, 1)),
    expires_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL,
    UNIQUE (chunk_id, reason)
);

-- Audit log (plan §12.1). PII rejections and forget events are recorded
-- with REASON AND CATEGORY ONLY — never the PII value (claude.md
-- principle 6): e.g. event = "rejected:pii", category = "email".
CREATE TABLE IF NOT EXISTS audit_log (
    owner_id TEXT,
    id TEXT PRIMARY KEY,
    event TEXT NOT NULL,
    category TEXT,
    detail TEXT,
    created_at TIMESTAMP NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_event ON audit_log(event);
