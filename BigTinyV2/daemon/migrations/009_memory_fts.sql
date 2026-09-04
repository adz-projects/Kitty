-- FTS5 index of `messages.content` for the pre-flight "detour": low-latency
-- recall of a session's already-compacted history. Uses `porter unicode61`
-- tokenization for natural-language BM25 relevance ranking (see
-- `agent::memory`).
--
-- `messages.id` is a UUID TEXT PRIMARY KEY, but the table is NOT
-- `WITHOUT ROWID`, so SQLite still maintains the implicit integer `rowid`
-- that every other query in this crate already keys on
-- (`get_messages_after_rowid`, fork remapping, MessageRow.rowid). That same
-- implicit `rowid` is the FTS link key (`content_rowid=rowid`), so
-- `NEW.rowid`/`OLD.rowid` in the triggers below resolve correctly.

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    session_id UNINDEXED,
    -- Keeps this table's rowids 1:1 with `messages.rowid` so lookups can
    -- join `messages_fts f` to `messages m` on `f.rowid = m.rowid` and honor
    -- the `compacted_through_rowid` watermark with `rowid <= ?`.
    content_rowid=rowid,
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages
WHEN NEW.role != 'system' AND NEW.content IS NOT NULL
BEGIN
    INSERT INTO messages_fts(rowid, content, session_id)
    VALUES (NEW.rowid, NEW.content, NEW.session_id);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, session_id)
    VALUES ('delete', OLD.rowid, OLD.content, OLD.session_id);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_au AFTER UPDATE ON messages
BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, session_id)
    VALUES ('delete', OLD.rowid, OLD.content, OLD.session_id);
    INSERT INTO messages_fts(rowid, content, session_id)
    VALUES (NEW.rowid, NEW.content, NEW.session_id);
END;

-- Backfill pre-existing rows (the migration only runs once, on daemon
-- startup / first open of an already-migrated DB). System rows are excluded
-- to match the INSERT trigger and to keep recall over real dialogue.
INSERT INTO messages_fts(rowid, content, session_id)
SELECT rowid, content, session_id FROM messages
WHERE content IS NOT NULL AND role != 'system';
