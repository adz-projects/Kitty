-- Content-addressed response cache.
--
-- Distinct from the daemon's `cache` config, which is about prompt-*prefix*
-- determinism so a provider's KV cache hits across turns. That makes the same
-- call cheaper; this makes a repeat call free.
--
-- The key is a hash of everything that can change a response -- app, provider,
-- model, messages, tools, sampling, response schema -- so a hit is only ever
-- served for a request generated under identical conditions. The app id is in
-- the key rather than in a WHERE clause: serving one app a response derived
-- from another's prompt would be a silent data leak, and keying it makes the
-- collision impossible rather than merely filtered out.
CREATE TABLE IF NOT EXISTS response_cache (
    key        TEXT PRIMARY KEY,
    response   TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT NOT NULL
);

-- Drives the expiry sweep; the PK already covers lookups.
CREATE INDEX IF NOT EXISTS idx_response_cache_expires ON response_cache(expires_at);
