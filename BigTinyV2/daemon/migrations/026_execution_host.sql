-- Where a run actually ran.
--
-- For a specialist run (`trigger_type = 'subagent'`) the host is chosen per
-- run and can move mid-run on failover, so the definition's pin says what was
-- asked for, not what answered. The orchestrator already reads back the host a
-- run really used (`Orchestrator::host_actually_used`) for its status events;
-- recording it here lets a client show it next to the run afterwards.
--
-- NULL for rows written before this migration and for runs whose host is
-- not recorded.

ALTER TABLE execution_history ADD COLUMN provider_id TEXT;
ALTER TABLE execution_history ADD COLUMN model TEXT;
