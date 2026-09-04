-- FK hygiene: `execution_history.session_id` and `schedule_jobs.recipe_id`
-- were created with no ON DELETE clause (unlike `messages.session_id`, which
-- is `ON DELETE CASCADE`), so:
--   * `DELETE /api/chat/{id}` on a session that has any execution_history
--     rows (scheduled/recipe runs) raised SQLITE_CONSTRAINT_FOREIGNKEY → 500,
--   * `DELETE /api/recipes/{id}` on a recipe still referenced by a schedule
--     did the same.
-- SQLite can't ALTER an existing column's FK action, so recreate both tables
-- with `ON DELETE CASCADE`, copying existing rows. Deleting a recipe cascades
-- to its schedules (a schedule pointing at a nonexistent recipe is useless);
-- deleting a session cascades to its execution history.

CREATE TABLE IF NOT EXISTS execution_history_v2 (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    trigger_type TEXT NOT NULL CHECK(trigger_type IN ('manual', 'schedule', 'recipe', 'subagent')),
    trigger_id TEXT,
    status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'cancelled')),
    started_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMP,
    result_summary TEXT,
    error_message TEXT
);

INSERT OR IGNORE INTO execution_history_v2 (
    id, session_id, trigger_type, trigger_id, status, started_at, completed_at, result_summary, error_message
)
SELECT id, session_id, trigger_type, trigger_id, status, started_at, completed_at, result_summary, error_message
FROM execution_history;

DROP TABLE execution_history;
ALTER TABLE execution_history_v2 RENAME TO execution_history;

CREATE TABLE IF NOT EXISTS schedule_jobs_v2 (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    cron TEXT NOT NULL,
    recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
    parameters TEXT,
    enabled INTEGER DEFAULT 1,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT OR IGNORE INTO schedule_jobs_v2 (
    id, name, cron, recipe_id, parameters, enabled, created_at, updated_at
)
SELECT id, name, cron, recipe_id, parameters, enabled, created_at, updated_at
FROM schedule_jobs;

DROP TABLE schedule_jobs;
ALTER TABLE schedule_jobs_v2 RENAME TO schedule_jobs;

CREATE INDEX IF NOT EXISTS idx_execution_session ON execution_history(session_id);

-- `get_recent_timings` orders by `created_at` (second-granularity, so several
-- LLM calls in the same second can tie) — `(session_id, created_at)` lets both
-- the WHERE and the ORDER BY use one index.
CREATE INDEX IF NOT EXISTS idx_llm_timings_session_created
    ON llm_timings(session_id, created_at);

