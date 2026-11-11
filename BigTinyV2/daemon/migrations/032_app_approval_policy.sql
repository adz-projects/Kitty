-- Per-app automatic approval policy (`agent::approval_policy`): which tool
-- calls the engine may approve itself instead of pausing for the app. No row
-- means none: every call pauses as before.

CREATE TABLE IF NOT EXISTS app_approval_policy (
    app_id     TEXT PRIMARY KEY,
    config     TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
