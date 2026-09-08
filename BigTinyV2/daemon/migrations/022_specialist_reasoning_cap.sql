-- How much of a delegate's run may go on reasoning.
--
-- Two columns rather than one JSON blob because they are two different
-- questions, and the UI asks them differently: a pipeline that has measured its
-- own workload wants an absolute number of tokens, while a definition that has
-- to work across an 8k local model and a 200k hosted one wants a share of
-- whatever room there is. Exactly one is set; both NULL means "use the daemon
-- default" (`agent.specialist_reasoning_fraction`), which is what every seeded
-- built-in does.
--
-- Nothing here is the enforcement. Only Anthropic and OpenRouter accept a
-- reasoning budget on the wire; the mechanism that works on all five dialects is
-- the per-run accumulator in `agent::loop_`, and these columns only tell it what
-- number to hold the run to.

ALTER TABLE specialists ADD COLUMN reasoning_cap_tokens INTEGER;
ALTER TABLE specialists ADD COLUMN reasoning_cap_fraction REAL;
