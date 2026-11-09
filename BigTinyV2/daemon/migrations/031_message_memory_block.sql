-- The memory recall block injected before a user message, stored on that
-- message so later turns re-emit it at the same position and the prompt
-- prefix stays byte-identical across turns (prompt_determinism.md). NULL on
-- every other row, and on a user row whose block repeated the previous one.
ALTER TABLE messages ADD COLUMN memory_block TEXT;
