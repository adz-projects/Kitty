-- Per-app choice of the model memory learning runs on (pathway extraction
-- and synthesis, memorabilia extraction): `agent::learning_model`. No row
-- means `auto` (the local helper model, else the chat's own provider).

CREATE TABLE IF NOT EXISTS app_learning_model (
    app_id     TEXT PRIMARY KEY,
    config     TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
