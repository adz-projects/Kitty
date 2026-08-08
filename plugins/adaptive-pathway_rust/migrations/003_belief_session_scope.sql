-- Conversation-layer beliefs are session-scoped ("Lives for the session" per
-- the three-layer model) -- context/identity beliefs are cross-session by
-- design and leave this column NULL. Without this, consolidation and recall
-- both operated over every session's conversation-layer beliefs at once.
ALTER TABLE beliefs ADD COLUMN session_id TEXT;

CREATE INDEX IF NOT EXISTS idx_beliefs_session ON beliefs(session_id);
