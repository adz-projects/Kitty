-- The folder a schedule's runs work in.
--
-- A run is a fresh session each time, and a session with no working folder
-- has nowhere its file tools may write without asking - which, for a run
-- nobody is watching, means waiting out the approval timeout and carrying on
-- without the tool. With `cwd` set, each run's session starts there (as its
-- `cwd` and `chat_dir`, the same pair a session created with a folder gets).
-- NULL keeps the old behaviour.

ALTER TABLE schedule_jobs ADD COLUMN cwd TEXT;
