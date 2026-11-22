-- Per-user-turn structured memory recall trace for the UI Memory Inspector
-- (kept separate from `memory_block` so prompt-prefix re-emission stays
-- byte-identical; see `agent::context::builder::build_messages`).
ALTER TABLE messages ADD COLUMN memory_trace TEXT;
