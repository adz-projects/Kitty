-- 001_init.sql — minimal initial schema (Phase 0).
--
-- Everything here is reserved state that later phases read/write. The full
-- Level 0 / Level 1 schema (documents, chunks, chunks_vec, propositions,
-- edges, registry, outbox, tombstones, audit) lands in migration 002+ per
-- project-plan.md §15 Phase 2.

-- Key/value store for persisted scheduler cadence (e.g. last_maintenance_at,
-- per §11) so the 24h heavy-pass gate survives restarts.
CREATE TABLE IF NOT EXISTS app_settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
