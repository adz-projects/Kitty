-- FTS5 external-content index over proposition claims for exact lexical
-- recall alongside dense vector search (RRF hybrid retrieval).

CREATE VIRTUAL TABLE IF NOT EXISTS propositions_fts USING fts5(
    claim,
    content='propositions',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS propositions_fts_ai
AFTER INSERT ON propositions BEGIN
    INSERT INTO propositions_fts(rowid, claim) VALUES (new.rowid, new.claim);
END;

CREATE TRIGGER IF NOT EXISTS propositions_fts_ad
AFTER DELETE ON propositions BEGIN
    INSERT INTO propositions_fts(propositions_fts, rowid, claim)
    VALUES ('delete', old.rowid, old.claim);
END;

CREATE TRIGGER IF NOT EXISTS propositions_fts_au
AFTER UPDATE OF claim ON propositions BEGIN
    INSERT INTO propositions_fts(propositions_fts, rowid, claim)
    VALUES ('delete', old.rowid, old.claim);
    INSERT INTO propositions_fts(rowid, claim)
    VALUES (new.rowid, new.claim);
END;

INSERT INTO propositions_fts(propositions_fts) VALUES('rebuild');
