-- Make `execution_history.session_id` nullable.
--
-- A scheduled run creates a throwaway `_job_<exec_id>` session purely so the
-- execution row has something to point at. The success path deletes it; the
-- failure path could not, because the NOT NULL FK made that row the session's
-- only anchor — so every failed scheduled run leaked a session plus its whole
-- message batch, permanently. On a phone that is unbounded growth driven by
-- exactly the runs the user least wants to pay for.
--
-- With the column nullable, the failure path can null it out and delete the
-- temp session (cascading its messages) while keeping the audit row, error
-- message and timestamps intact.
--
-- SQLite cannot drop a NOT NULL constraint in place, so recreate the table
-- (same shape and ON DELETE CASCADE as migration 012, minus the NOT NULL).

CREATE TABLE IF NOT EXISTS execution_history_v3 (
    id TEXT PRIMARY KEY,
    session_id TEXT REFERENCES sessions(id) ON DELETE CASCADE,
    trigger_type TEXT NOT NULL CHECK(trigger_type IN ('manual', 'schedule', 'recipe', 'subagent')),
    trigger_id TEXT,
    status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'cancelled')),
    started_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMP,
    result_summary TEXT,
    error_message TEXT
);

INSERT OR IGNORE INTO execution_history_v3 (
    id, session_id, trigger_type, trigger_id, status, started_at, completed_at, result_summary, error_message
)
SELECT id, session_id, trigger_type, trigger_id, status, started_at, completed_at, result_summary, error_message
FROM execution_history;

DROP TABLE execution_history;
ALTER TABLE execution_history_v3 RENAME TO execution_history;

CREATE INDEX IF NOT EXISTS idx_execution_session ON execution_history(session_id);
CREATE INDEX IF NOT EXISTS idx_execution_trigger ON execution_history(trigger_id);

-- Retroactively clear the leak: every temp job session still anchored by a
-- finished execution row is dead weight. Null the anchor, then delete the
-- sessions (their messages cascade).
UPDATE execution_history
   SET session_id = NULL
 WHERE session_id LIKE '\_job\_%' ESCAPE '\'
   AND status IN ('completed', 'failed', 'cancelled');

DELETE FROM sessions
 WHERE id LIKE '\_job\_%' ESCAPE '\'
   AND id NOT IN (SELECT session_id FROM execution_history WHERE session_id IS NOT NULL);
