ALTER TABLE sessions ADD COLUMN memory_slots TEXT;
ALTER TABLE sessions ADD COLUMN compacted_through_rowid INTEGER DEFAULT 0;
ALTER TABLE sessions ADD COLUMN compaction_state TEXT DEFAULT 'idle';
ALTER TABLE sessions ADD COLUMN compaction_started_at TIMESTAMP;
