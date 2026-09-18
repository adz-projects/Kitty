-- Phase 6 (plan §15): per-chunk extraction retry backoff (plan §3.6).
-- The pending queue is the extraction_status flag itself; this column is
-- the failure half of that pair. NULL = no failure in flight; set to the
-- UTC timestamp of the last failed attempt on extraction failure and
-- cleared (with extraction_status -> 'done') on success. A chunk is
-- eligible for retry once
--   extraction_error_at <= now - extraction.retry_backoff_s
-- so one persistently failing chunk backs off on its own instead of
-- blocking the oldest-first drain. ISO-8601 UTC strings compare
-- lexicographically = chronologically, so the bound is a plain string
-- comparison.

ALTER TABLE chunks ADD COLUMN extraction_error_at TIMESTAMP;
