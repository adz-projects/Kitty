-- `get_executions_for_recipe` filters `execution_history` by `trigger_id`,
-- which had no index (only `idx_execution_session` on `session_id`, from
-- 001/012) — every recipe-history read was a full scan of a forever-growing
-- table.
CREATE INDEX IF NOT EXISTS idx_execution_trigger ON execution_history(trigger_id);
