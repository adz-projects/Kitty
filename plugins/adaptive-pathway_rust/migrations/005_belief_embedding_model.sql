-- Tags each belief with the embedding model that produced its `embedding`
-- BLOB. Without this, switching the configured embedding model (Settings,
-- or a config edit) leaves every existing belief's embedding in the OLD
-- model's semantic space while new query embeddings land in the NEW
-- model's space -- cosine similarity between the two is meaningless, and
-- nothing would ever surface an error; recall would just silently degrade
-- to comparing incomparable vectors.
--
-- Empty-string default is a deliberate "needs re-embedding" sentinel for
-- rows that predate this column, the same conservative bucket a genuine
-- model-name mismatch falls into (see `PathwayEngine::open`'s fingerprint
-- check and `background::reembed_stale_beliefs`).
ALTER TABLE beliefs ADD COLUMN embedding_model TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_beliefs_embedding_model ON beliefs(embedding_model);
