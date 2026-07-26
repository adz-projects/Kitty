from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import aiosqlite

from bigtiny import paths


MIGRATION_V001 = """
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    name TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    status TEXT DEFAULT 'active' CHECK(status IN ('active', 'idle', 'failed', 'archived')),
    metadata TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK(role IN ('user', 'assistant', 'system', 'tool')),
    content TEXT,
    tool_calls TEXT,
    token_count INTEGER DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS hitl_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tool_name TEXT NOT NULL,
    args_pattern TEXT,
    decision TEXT NOT NULL CHECK(decision IN ('allow', 'always_allow', 'reject')),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS providers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    provider_type TEXT NOT NULL CHECK(provider_type IN ('openai_compat', 'anthropic')),
    base_url TEXT NOT NULL,
    fallback_priority INTEGER DEFAULT 1,
    config TEXT,
    status TEXT DEFAULT 'disconnected' CHECK(status IN ('connected', 'disconnected', 'error')),
    error_message TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS mcp_servers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    transport TEXT NOT NULL CHECK(transport IN ('stdio', 'sse')),
    command TEXT,
    args TEXT,
    sse_url TEXT,
    env TEXT,
    status TEXT DEFAULT 'disconnected' CHECK(status IN ('connected', 'disconnected', 'error')),
    error_message TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS recipes (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    prompt_template TEXT NOT NULL,
    instructions TEXT,
    parameters TEXT,
    required_mcp_servers TEXT,
    system_prompt_layer TEXT,
    max_steps INTEGER DEFAULT 30,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS schedule_jobs (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    cron TEXT NOT NULL,
    recipe_id TEXT NOT NULL REFERENCES recipes(id),
    parameters TEXT,
    enabled INTEGER DEFAULT 1,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS execution_history (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    trigger_type TEXT NOT NULL CHECK(trigger_type IN ('manual', 'schedule', 'recipe', 'subagent')),
    trigger_id TEXT,
    status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'failed', 'cancelled')),
    started_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMP,
    result_summary TEXT,
    error_message TEXT
);

CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_execution_session ON execution_history(session_id);
"""

MIGRATION_V002 = """
ALTER TABLE messages ADD COLUMN tool_call_id TEXT;
"""

# content_format: 'text' = content column is plain text;
# 'blocks' = content column is a JSON array of content blocks (multimodal).
MIGRATION_V003 = """
ALTER TABLE messages ADD COLUMN content_format TEXT DEFAULT 'text';
"""

MIGRATION_V004 = """
ALTER TABLE mcp_servers ADD COLUMN enabled INTEGER DEFAULT 1;
"""

# Adds the "streamable_http" transport (the MCP spec's successor to the old
# two-endpoint HTTP+SSE transport — a single POST endpoint, response framed
# as either plain JSON or one `event: message` SSE frame) and generic
# `headers` support for authenticating with a remote MCP server (neither the
# `sse` nor the new `streamable_http` transport could send any auth header
# before this — confirmed real need: a personal RAG MCP server requiring a
# Bearer token). SQLite can't ALTER a CHECK constraint or rename a column
# in one step pre-3.25, so this recreates the table rather than a plain
# ALTER; `sse_url` becomes `url` since it's no longer SSE-transport-specific.
MIGRATION_V005 = """
CREATE TABLE mcp_servers_new (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    transport TEXT NOT NULL CHECK(transport IN ('stdio', 'sse', 'streamable_http')),
    command TEXT,
    args TEXT,
    url TEXT,
    env TEXT,
    headers TEXT,
    enabled INTEGER DEFAULT 1,
    status TEXT DEFAULT 'disconnected' CHECK(status IN ('connected', 'disconnected', 'error')),
    error_message TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO mcp_servers_new
    (id, name, transport, command, args, url, env, enabled, status, error_message, created_at, updated_at)
SELECT id, name, transport, command, args, sse_url, env, enabled, status, error_message, created_at, updated_at
FROM mcp_servers;
DROP TABLE mcp_servers;
ALTER TABLE mcp_servers_new RENAME TO mcp_servers;
"""

# Conversation compaction state. `messages.id` is a uuid4 hex with no
# inherent ordering and `created_at` only has 1-second resolution (see the
# rowid-tiebreaker comment on the history query in context_manager.py), so
# `rowid` — SQLite's own monotonic row sequence — is the only ordering
# compaction can key its watermark on.
#
# compaction_state guards against two background passes for the same
# session running concurrently; compaction_started_at lets a pass that
# crashed mid-run (state stuck at 'running') be reclaimed after it goes
# stale, rather than wedging that session's compaction forever.
MIGRATION_V006 = """
ALTER TABLE sessions ADD COLUMN memory_slots TEXT;
ALTER TABLE sessions ADD COLUMN compacted_through_rowid INTEGER DEFAULT 0;
ALTER TABLE sessions ADD COLUMN compaction_state TEXT DEFAULT 'idle';
ALTER TABLE sessions ADD COLUMN compaction_started_at TIMESTAMP;
"""

# Perf indexes, added after auditing the daemon's hot-path query patterns:
# - `sessions(updated_at)`: `list_sessions` orders by this with no supporting
#   index today (full scan + sort on every session-list call).
# - `hitl_rules(tool_name)`: `_check_db_rules` filters on this on every
#   single tool call, every turn — currently a full table scan per call.
#
# A `messages(session_id, rowid)` composite was considered too, for the
# `WHERE session_id=:sid AND rowid > :through ORDER BY rowid ASC` queries in
# context_manager.py/compaction.py — but SQLite doesn't accept the rowid
# alias as an explicit indexed column (`CREATE INDEX ... ON t(rowid)` raises
# "no such column: rowid"). It isn't needed anyway: `idx_messages_session`
# already lets SQLite locate a session's rows via the index, and since a
# plain rowid table's underlying storage is itself ordered by rowid, that
# lookup already yields rows in rowid order for the `ORDER BY rowid ASC`
# clause without an extra sort step.
MIGRATION_V007 = """
CREATE INDEX IF NOT EXISTS idx_sessions_updated_at ON sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_hitl_rules_tool_name ON hitl_rules(tool_name);
"""

MIGRATIONS: dict[int, str] = {
    1: MIGRATION_V001,
    2: MIGRATION_V002,
    3: MIGRATION_V003,
    4: MIGRATION_V004,
    5: MIGRATION_V005,
    6: MIGRATION_V006,
    7: MIGRATION_V007,
}


class Database:
    def __init__(self, db_path: str | None = None):
        resolved = db_path if db_path is not None else str(Path(paths.data_dir()) / "bigtiny.db")
        self.db_path = str(Path(resolved).expanduser())
        self._conn: aiosqlite.Connection | None = None

    async def connect(self) -> None:
        parent = Path(self.db_path).parent
        parent.mkdir(parents=True, exist_ok=True)

        # Autocommit mode: without it aiosqlite opens implicit transactions
        # that are never committed, so every write is lost on close.
        self._conn = await aiosqlite.connect(
            self.db_path, timeout=5, isolation_level=None
        )
        self._conn.row_factory = aiosqlite.Row

        await self._conn.execute("PRAGMA journal_mode=WAL")
        await self._conn.execute("PRAGMA foreign_keys=ON")
        await self._run_migrations()

    async def _run_migrations(self) -> None:
        await self.conn.execute(
            "CREATE TABLE IF NOT EXISTS schema_version ("
            "  version INTEGER PRIMARY KEY,"
            "  applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP"
            ")"
        )
        current = await self.fetch_one(
            "SELECT COALESCE(MAX(version), 0) as v FROM schema_version"
        )
        applied_version = current["v"] if current else 0

        for version, sql in sorted(MIGRATIONS.items()):
            if version > applied_version:
                await self.conn.executescript(sql)
                await self.execute(
                    "INSERT INTO schema_version (version) VALUES (:v)",
                    {"v": version},
                )

    @property
    def conn(self) -> aiosqlite.Connection:
        assert self._conn is not None, "Database not connected. Call connect() first."
        return self._conn

    async def execute(self, sql: str, params: dict[str, Any] | None = None) -> aiosqlite.Cursor:
        return await self.conn.execute(sql, params or {})

    async def fetch_one(self, sql: str, params: dict[str, Any] | None = None) -> dict[str, Any] | None:
        cursor = await self.conn.execute(sql, params or {})
        row = await cursor.fetchone()
        if row is None:
            return None
        return dict(row)

    async def fetch_all(self, sql: str, params: dict[str, Any] | None = None) -> list[dict[str, Any]]:
        cursor = await self.conn.execute(sql, params or {})
        rows = await cursor.fetchall()
        return [dict(r) for r in rows]

    async def close(self) -> None:
        if self._conn:
            await self._conn.close()
            self._conn = None
