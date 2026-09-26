-- What an assistant message was produced by, and the reasoning behind it.
--
-- `reasoning` is the model's streamed thinking for that message. It was only
-- ever token-counted and then dropped, so a client reloading a conversation
-- could not show it, and an export could not include it.
--
-- `provider_id`/`model` record which provider and model *actually* produced
-- the message. A session's pin says what was asked for; failover can answer
-- with something else mid-conversation, and a per-turn record is the only way
-- an export or a client can say what really spoke.
--
-- All NULL for user/tool rows and for rows written before this migration.
-- Never sent back to a provider: the in-memory form carries them under
-- `_`-prefixed keys, which `provider::wire::sanitize_for_wire` strips.

ALTER TABLE messages ADD COLUMN reasoning TEXT;
ALTER TABLE messages ADD COLUMN provider_id TEXT;
ALTER TABLE messages ADD COLUMN model TEXT;
