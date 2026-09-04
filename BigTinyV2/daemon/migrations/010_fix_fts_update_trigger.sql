-- Fix the FTS5 update-trigger guard added in 009. The original
-- `messages_fts_au` deleted-then-inserted unconditionally, so updating a
-- non-system row to `system`/`NULL` content would still index it — diverging
-- from the INSERT trigger's `WHEN ... != 'system' AND content IS NOT NULL`
-- guard (and the backfill's). A single trigger can't apply a different WHEN
-- per statement, so split into two guarded triggers: one to delete the OLD
-- (only if it was indexed), one to insert the NEW (only if it should be).
--
-- Dropping and recreating rather than altering keeps fanout idempotent and
-- leaves existing 009 triggers replaced in favour of these.

DROP TRIGGER IF EXISTS messages_fts_au;

CREATE TRIGGER IF NOT EXISTS messages_fts_au_del AFTER UPDATE ON messages
WHEN OLD.role != 'system' AND OLD.content IS NOT NULL
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, session_id)
    VALUES ('delete', OLD.rowid, OLD.content, OLD.session_id);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_au_ins AFTER UPDATE ON messages
WHEN NEW.role != 'system' AND NEW.content IS NOT NULL
BEGIN
    INSERT INTO messages_fts(rowid, content, session_id)
    VALUES (NEW.rowid, NEW.content, NEW.session_id);
END;