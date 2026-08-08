-- FTS5 index over belief text for search (the Settings belief browser).
-- `content_rowid` only has meaning paired with `content=` (it tells FTS5
-- which *external* table's rowid space to defer to) -- specifying it alone,
-- as the original version of this migration did, leaves the index in
-- FTS5's default self-managed rowid space, which does not correspond to
-- `beliefs.rowid`. The triggers below all assume that correspondence (they
-- key the special `('delete', rowid, ...)` command off `beliefs`' own OLD/
-- NEW.rowid), so without `content='beliefs'` every `UPDATE`/`DELETE` on
-- `beliefs` fails outright: the 'delete' pseudo-row targets a rowid that
-- was never inserted into `beliefs_fts`'s own separate key space.
CREATE VIRTUAL TABLE IF NOT EXISTS beliefs_fts USING fts5(
    text,
    content='beliefs',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS beliefs_fts_ai AFTER INSERT ON beliefs
WHEN NEW.text IS NOT NULL
BEGIN
    INSERT INTO beliefs_fts(rowid, text) VALUES (NEW.rowid, NEW.text);
END;

CREATE TRIGGER IF NOT EXISTS beliefs_fts_ad AFTER DELETE ON beliefs
BEGIN
    INSERT INTO beliefs_fts(beliefs_fts, rowid, text)
    VALUES ('delete', OLD.rowid, OLD.text);
END;

CREATE TRIGGER IF NOT EXISTS beliefs_fts_au AFTER UPDATE ON beliefs
BEGIN
    INSERT INTO beliefs_fts(beliefs_fts, rowid, text)
    VALUES ('delete', OLD.rowid, OLD.text);
    INSERT INTO beliefs_fts(rowid, text) VALUES (NEW.rowid, NEW.text);
END;

INSERT INTO beliefs_fts(rowid, text)
SELECT rowid, text FROM beliefs WHERE text IS NOT NULL;
