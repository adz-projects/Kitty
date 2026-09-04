CREATE TABLE IF NOT EXISTS llm_timings (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    provider_id TEXT,
    model TEXT,
    ttfb_ms REAL,
    ttft_ms REAL,
    generation_ms REAL,
    total_tokens INTEGER,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_llm_timings_session ON llm_timings(session_id);
