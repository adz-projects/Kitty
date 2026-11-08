-- Per-app overnight learning (`crate::overnight`, docs/MEMORY_OVERNIGHT.md in
-- Kitty): when an app's memory engines do their expensive work, and what the
-- last pass did.
--
-- No row means "no window": memory work runs on every sweep tick, as before.

CREATE TABLE IF NOT EXISTS app_overnight (
    app_id        TEXT PRIMARY KEY,
    config        TEXT NOT NULL,
    last_started  TEXT,
    last_finished TEXT,
    last_report   TEXT,
    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
);
