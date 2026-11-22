-- Phase 3 & 4: cross-session staged evidence before Context promotion,
-- behavioral signal feedback telemetry, and novelty cooldown tracking.

CREATE TABLE IF NOT EXISTS staged_evidence (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    statement TEXT NOT NULL,
    kind TEXT NOT NULL,
    provenance TEXT NOT NULL,
    domain TEXT,
    evidence TEXT,
    embedding BLOB NOT NULL,
    embedding_model TEXT NOT NULL,
    created_at TEXT NOT NULL,
    synthesized_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_staged_evidence_pending
    ON staged_evidence(synthesized_at, created_at);

CREATE TABLE IF NOT EXISTS signal_feedback (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    signal_kind TEXT NOT NULL,
    engaged INTEGER NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_signal_feedback_kind
    ON signal_feedback(signal_kind, created_at);

ALTER TABLE conversation_state
    ADD COLUMN last_novelty_turn INTEGER NOT NULL DEFAULT -100;
