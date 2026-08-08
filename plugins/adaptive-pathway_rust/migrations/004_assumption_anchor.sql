-- `exchanged_since_flag` was designed as a running counter, re-stamped on
-- every state-machine advance call -- but that ties "how long has this gone
-- untested" to how often the advance function happens to be invoked, not to
-- actual exchange volume. Renamed to `flagged_at_exchange`: the global
-- exchange counter's value at flag time, a fixed anchor. Elapsed exchanges
-- are computed live as `global_exchange_count() - flagged_at_exchange`,
-- never re-stamped.
ALTER TABLE assumptions RENAME COLUMN exchanged_since_flag TO flagged_at_exchange;
