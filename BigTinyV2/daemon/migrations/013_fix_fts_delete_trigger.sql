-- Fix the FTS5 triggers that remove rows from the index. Two defects, one of
-- which made whole sessions undeletable.
--
-- 1. THE BREAKING ONE. Both removal triggers used fts5's special
--    `INSERT INTO messages_fts(messages_fts, rowid, ...) VALUES ('delete', ...)`
--    command. That command exists only for **external-content** and
--    **contentless** FTS5 tables — ones declared with a `content=` option.
--    `messages_fts` (009) has no `content=`, so it is an ordinary FTS5 table
--    that stores its own copy of the text, and against an ordinary table the
--    command raises SQLITE_ERROR ("SQL logic error"). Verified directly
--    against SQLite: the same table declaration accepts a plain
--    `DELETE FROM ... WHERE rowid = ?` and rejects the `'delete'` command.
--
--    (`content_rowid=rowid` in 009 is what makes this look like an
--    external-content table at a glance. It isn't: `content_rowid` is only
--    meaningful alongside `content=`, and on its own it is inert — it names
--    the rowid column, which is already the default.)
--
--    The consequence: deleting a session cascades to `messages`, the trigger
--    fires for each indexed row, and the raise aborts the whole transaction.
--    A session with at least one ordinary user/assistant message could not be
--    deleted at all — the API answered 500 and Kitty's UI dropped the error on
--    the floor, so it read as "the delete button does nothing." Sessions with
--    nothing indexed (empty, or only `system` rows) deleted fine, which is why
--    it presented as "*some* chats won't delete."
--
-- 2. The missing guard. `messages_fts_ad` had no `WHEN` clause, while the
--    insert trigger only indexes `role != 'system' AND content IS NOT NULL`.
--    010 fixed exactly this asymmetry for the update trigger and left delete
--    alone. Harmless once (1) is fixed — deleting an unindexed rowid is a
--    no-op — but it should match its sibling, and a trigger that lies about
--    which rows it covers is a trap for the next person.
--
-- Dropping and recreating, matching 010's approach.

DROP TRIGGER IF EXISTS messages_fts_ad;

CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages
WHEN OLD.role != 'system' AND OLD.content IS NOT NULL
BEGIN
    DELETE FROM messages_fts WHERE rowid = OLD.rowid;
END;

-- Same invalid command, same fix: an UPDATE to an indexed message raised
-- rather than reindexing. 010 introduced this one and inherited the bug from
-- 009's original.
DROP TRIGGER IF EXISTS messages_fts_au_del;

CREATE TRIGGER IF NOT EXISTS messages_fts_au_del AFTER UPDATE ON messages
WHEN OLD.role != 'system' AND OLD.content IS NOT NULL
BEGIN
    DELETE FROM messages_fts WHERE rowid = OLD.rowid;
END;
