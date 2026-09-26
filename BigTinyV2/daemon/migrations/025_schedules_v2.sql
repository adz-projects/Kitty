-- Schedules v2: timing kinds, what a run answers with, and what the last run did.
--
-- `kind` is `cron` (the only kind until now), `interval` (every
-- `interval_secs`, from the last run) or `once` (a single run at `run_at`,
-- after which the schedule disables itself). For the two timer kinds,
-- `next_run_at` is persisted so a daemon that was not running when a run fell
-- due catches up on its next start instead of silently skipping it. `cron`
-- stays NOT NULL for compatibility and is '' for the timer kinds.
--
-- `provider_id`/`model` pin a run to a provider (a session's usual pin);
-- `system_prompt` becomes its persona; `hitl_timeout_secs` is how long a run
-- waits for someone to approve a tool before carrying on without it. Nobody is
-- watching a scheduled run by default, so the wait is minutes, not the hour an
-- interactive session gets.
--
-- `last_run_at`/`last_status`/`last_session_id` summarize the latest run so a
-- client can show it without paging `execution_history`.

ALTER TABLE schedule_jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'cron';
ALTER TABLE schedule_jobs ADD COLUMN interval_secs INTEGER;
ALTER TABLE schedule_jobs ADD COLUMN run_at TEXT;
ALTER TABLE schedule_jobs ADD COLUMN next_run_at TEXT;
ALTER TABLE schedule_jobs ADD COLUMN provider_id TEXT;
ALTER TABLE schedule_jobs ADD COLUMN model TEXT;
ALTER TABLE schedule_jobs ADD COLUMN system_prompt TEXT;
ALTER TABLE schedule_jobs ADD COLUMN hitl_timeout_secs INTEGER NOT NULL DEFAULT 600;
ALTER TABLE schedule_jobs ADD COLUMN last_run_at TEXT;
ALTER TABLE schedule_jobs ADD COLUMN last_status TEXT;
ALTER TABLE schedule_jobs ADD COLUMN last_session_id TEXT;
