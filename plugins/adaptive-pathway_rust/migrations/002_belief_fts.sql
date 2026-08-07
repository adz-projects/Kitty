-- FTS5 index over belief text for search (the Settings belief browser).
CREATE VIRTUAL TABLE IF NOT EXISTS beliefs_fts USING fts5(
    text,
    content_rowid=rowid,
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
