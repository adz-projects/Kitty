-- Per-app decision-model configuration (`crate::decider`).
--
-- One JSON document per app: the endpoint, its dialect, which features are
-- on, thresholds and the app's tool-group catalog. Its own table rather than a
-- column on `apps` because it is optional, app-owned and replaced wholesale --
-- nothing about it is read on the auth path that selects `apps` rows.
--
-- No row means "no decision model": every feature behaves exactly as before.

CREATE TABLE IF NOT EXISTS app_decider (
    app_id     TEXT PRIMARY KEY,
    config     TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
