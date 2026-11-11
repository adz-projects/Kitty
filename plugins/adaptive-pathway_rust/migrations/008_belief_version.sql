-- A counter that moves on every change to `beliefs`, so recall can keep the
-- decoded candidate set in memory and re-read it only when a belief changed
-- (Db::list_recall_candidates).
CREATE TABLE IF NOT EXISTS belief_version (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    n  INTEGER NOT NULL
);
INSERT OR IGNORE INTO belief_version (id, n) VALUES (1, 0);

CREATE TRIGGER IF NOT EXISTS beliefs_version_insert AFTER INSERT ON beliefs
BEGIN UPDATE belief_version SET n = n + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS beliefs_version_update AFTER UPDATE ON beliefs
BEGIN UPDATE belief_version SET n = n + 1 WHERE id = 1; END;
CREATE TRIGGER IF NOT EXISTS beliefs_version_delete AFTER DELETE ON beliefs
BEGIN UPDATE belief_version SET n = n + 1 WHERE id = 1; END;
