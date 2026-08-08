-- Batch co-occurrence.
--
-- `learn::extract_and_record` emits up to 5 observations from a single
-- extraction pass over one stretch of conversation. Those observations are
-- jointly meaningful -- "the MSRV is 1.70", "the pool caps at 8 connections",
-- "proc-macros are banned in this crate" are one problem context, not three
-- unrelated facts -- but each is routed to a belief independently and the
-- relation between them is then lost forever.
--
-- Cosine similarity cannot recover it: co-occurring constraints are usually
-- semantically *distant* from each other, which is exactly why they don't
-- already cluster in the recall embedding space. `batch_id` records the
-- relation directly so recall can pull siblings in behind an anchor.
--
-- It lives on `observations`, not `beliefs`, deliberately. Observations merge
-- into pre-existing beliefs at cosine >= MERGE_COSINE (belief/synthesis.rs),
-- so a single belief accumulates observations from many extraction passes
-- over its lifetime. A `batch_id` column on `beliefs` would be overwritten by
-- each merge and end up meaning nothing; on `observations` -- which already
-- carries `belief_id` -- it gives a correct many-to-many sibling relation for
-- free, via a self-join on shared batch.
--
-- NULL means "no batch": observations written before this migration, and
-- single observations recorded through the `record` MCP tool, which by
-- definition have no siblings. Those are simply never sibling-matched.

ALTER TABLE observations ADD COLUMN batch_id TEXT;

CREATE INDEX IF NOT EXISTS idx_observations_batch ON observations(batch_id);
