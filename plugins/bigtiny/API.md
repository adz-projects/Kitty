# BigTiny Daemon — Configuration & API Reference

## Configuration

The daemon is configured via a YAML file (default: all-defaults). Pass a custom path with `--config`.

### Default config structure

```yaml
server:
  host: 127.0.0.1
  port: 8080
  reload: false

logging:
  level: info          # debug | info | warning | error
  json_format: true    # structured JSON logs

token_management:
  max_context_tokens: 128000
  compaction_threshold: 0.8     # compact when context exceeds 80% of limit
  compaction_target_ratio: 0.5  # compact down to 50%

fallback:
  mode: priority                  # priority | round-robin
  retry_on_error: true
  max_retries: 2
  retry_backoff_ms: 1000

hitl:
  default_policy: always_ask      # always_ask | auto_allow | auto_reject
  always_allow_patterns: []
  auto_reject_patterns:
    - "rm -rf /"
    - "chmod 777"
    - "dd if="
    - "mkfs"

recipes:
  directory: ~/.bigtiny/recipes   # directory for YAML recipe files

scheduler:
  enabled: true
```

### Sections

| Section | Fields |
|---------|--------|
| `server` | Bind address, port, hot-reload |
| `logging` | Verbosity level and structured-output toggle |
| `token_management` | Context-window budget and compaction thresholds |
| `fallback` | Provider failover strategy (`priority` = static order, `round-robin` = rotate) |
| `hitl` | Human-in-the-loop policy: default policy and pattern-based allow/reject lists |
| `recipes` | Directory scanned at startup for `.yaml`/`.yml` recipe files |
| `scheduler` | Master enable/disable for the cron scheduler |

### API key storage

API keys are stored in the OS keyring (not in files or SQLite). Each key is indexed as:

```
bigtiny_{provider_id}_api_key
```

Set a key from the command line:

```bash
python -m keyring set bigtiny <provider_id>_api_key
```

---

## Running

```bash
# Default (127.0.0.1:8080)
python -m bigtiny

# Custom address
python -m bigtiny --host 0.0.0.0 --port 9000

# With hot-reload (development)
python -m bigtiny --reload

# Custom config file
python -m bigtiny --config /path/to/config.yaml
```

### CLI arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--host` | `127.0.0.1` | Bind address |
| `--port` | `8080` | Bind port |
| `--reload` | `false` | Auto-reload on file changes (uvicorn) |
| `--config` | — | Path to YAML configuration file |
| `--secret` | — | API secret (same as setting `BIGTINY_SECRET`) |

### Authentication

If a secret is configured (`--secret` flag or `BIGTINY_SECRET` env var), every
`/api/*` request must carry it in the `X-API-Key` header; requests without it
get `401`. `GET /api/health` stays open so launchers can poll readiness.
With no secret configured, the API is open (local development).

---

## API Endpoints

### Health & Status

| Method | Endpoint | Description | Returns |
|--------|----------|-------------|---------|
| GET | `/api/health` | Lightweight health check | Provider health, MCP status, uptime, active sessions |
| GET | `/api/status` | Detailed system status | Session count, provider list, health breakdown |

**`GET /api/health`**

```json
{
  "status": "healthy",
  "providers": {
    "a1b2c3d4": { "status": "healthy", "latency_ms": 320.0 }
  },
  "mcp_servers": {},
  "uptime_sec": 187,
  "active_sessions": 0
}
```

**`GET /api/status`**

```json
{
  "sessions": 5,
  "active_sessions": 1,
  "providers": ["a1b2c3d4"],
  "mcp_servers": [],
  "provider_health": {
    "a1b2c3d4": { "status": "healthy", "latency_ms": 320.0 }
  }
}
```

---

### Chat Sessions

| Method | Endpoint | Description | Returns |
|--------|----------|-------------|---------|
| GET | `/api/chat/` | List all sessions | `{ "sessions": [...], "total": N }` |
| POST | `/api/chat/` | Create a new session | `{ "session_id": "..." }` |
| POST | `/api/chat/{id}/send` | Send a message (SSE stream) | `text/event-stream` |
| POST | `/api/chat/{id}/fork` | Branch a session | `{ "session_id": "...", "copied_messages": N }` |
| PATCH | `/api/chat/{id}/config` | Set per-session provider/model/persona | `{ "status": "updated", "config": {...} }` |
| PATCH | `/api/chat/{id}` | Rename a session | `{ "status": "updated" }` |
| GET | `/api/chat/{id}/pending` | List pending HITL approvals | `[{ "action_id": "...", ... }]` |
| POST | `/api/chat/{id}/approve` | Approve/reject a pending action | `{ "status": "approved" }` |
| POST | `/api/chat/{id}/cancel` | Cancel a running session | `{ "status": "cancelled" }` |
| GET | `/api/chat/{id}/history` | Get message history | `[{ "role": "...", "content": "...", ... }]` |
| GET | `/api/chat/{id}/stats` | Get session statistics | `{ "message_count": N, "total_tokens": N }` |
| DELETE | `/api/chat/{id}` | Delete a session | `{ "status": "deleted" }` |

**`POST /api/chat/`** — Create session

JSON body (all fields optional; legacy `name` query param still accepted):
```json
{ "name": "My chat", "cwd": "C:/Users/me/chats/session-folder" }
```
`cwd` is stored in session metadata for clients that maintain per-session
working folders.

```json
{ "session_id": "abc123def456" }
```

**`POST /api/chat/{id}/fork`** — Branch a session

Copies the session and its messages into a new session. With
`at_message_id`, copies only messages up to and including that message
(for "branch from here" / regenerate).

```json
{ "at_message_id": "msg_002" }
```

Returns `{ "session_id": "<new>", "copied_messages": N }`. The new session's
metadata records `forked_from`.

**`PATCH /api/chat/{id}/config`** — Per-session configuration

```json
{ "provider": "a1b2c3d4", "model": "qwen2.5:14b", "persona_override": "You are..." }
```

All fields optional; send `""` to clear an override. The agent reads these
from session metadata on every run — switching provider/model requires no
daemon restart.

**`POST /api/chat/{id}/send`** — Streams Server-Sent Events

Request body (`images` optional; base64 payloads):
```json
{
  "message": "What is in this screenshot?",
  "images": [{ "data": "<base64>", "mime_type": "image/png" }]
}
```

Response is an SSE stream of `SSEEvent` objects. Each event is a JSON line prefixed with `data: `:

```
data: {"type":"llm_delta","content":"Here","session_id":"abc...","is_last":false}

data: {"type":"llm_delta","content":" is the list","session_id":"abc...","is_last":false}

data: {"type":"llm_stop","session_id":"abc...","is_last":true}

data: {"type":"session_status","session_id":"abc...","is_last":true}
```

SSE event types:

| Type | Meaning |
|------|---------|
| `llm_delta` | Token chunk from the LLM |
| `reasoning_delta` | Thinking/reasoning token chunk (Anthropic `thinking`, OpenAI-compat `reasoning_content`) |
| `llm_stop` | LLM finished generating; carries `usage: {"input_tokens", "output_tokens"}` for the whole run |
| `session_title` | Auto-generated title for a previously untitled session (in `content`) |
| `tool_start` | Tool call began |
| `tool_finish` | Tool call completed (with result) |
| `hitl_pause` | Awaiting human approval; carries `action_id` (POST it back to `/approve`) plus `tool_name`/`tool_args` |
| `hitl_resolved` | Human decision received |
| `error` | An error occurred |
| `model_failover` | Router switched to fallback provider |
| `subagent_status` | Subagent status update |
| `session_status` | Session lifecycle event |

Every event includes an `is_last` field — the stream ends when `is_last: true`.

**`POST /api/chat/{id}/approve`**

```json
{ "action_id": "abc123", "decision": "allow" }
```

`decision` is one of: `allow`, `always_allow`, `reject`.

**`GET /api/chat/{id}/history?limit=50`**

```json
[
  {
    "id": "msg_001",
    "session_id": "abc123",
    "role": "user",
    "content": "What files are here?",
    "tool_calls": null,
    "token_count": 12,
    "created_at": "2026-07-23T12:00:00"
  },
  {
    "id": "msg_002",
    "session_id": "abc123",
    "role": "assistant",
    "content": "Here are the files: ...",
    "tool_calls": null,
    "token_count": 45,
    "created_at": "2026-07-23T12:00:01"
  }
]
```

**`GET /api/chat/{id}/stats`**

```json
{
  "session_id": "abc123",
  "message_count": 12,
  "total_tokens": 890
}
```

---

### Providers

| Method | Endpoint | Description | Returns |
|--------|----------|-------------|---------|
| GET | `/api/providers` | List all configured providers | `{ "providers": [...] }` |
| POST | `/api/providers` | Register a new provider | `{ "id": "...", "status": "created" }` |
| PATCH | `/api/providers/{id}` | Update provider config | `{ "status": "updated" }` |
| DELETE | `/api/providers/{id}` | Remove a provider (clears keyring) | `{ "status": "deleted" }` |
| POST | `/api/providers/{id}/test` | Test provider connectivity | `{ "status": "connected", "models": [...] }` |
| GET | `/api/providers/{id}/models` | List models from a provider | `{ "models": [...] }` |

**`POST /api/providers`**

```json
{
  "name": "My OpenAI",
  "provider_type": "openai_compat",
  "base_url": "https://api.openai.com/v1",
  "api_key": "sk-...",
  "fallback_priority": 1,
  "config": {}
}
```

`provider_type`: `openai_compat` | `anthropic`

Returns:
```json
{ "id": "a1b2c3d4", "status": "created" }
```

**`PATCH /api/providers/{id}`**

```json
{ "name": "Renamed Provider", "base_url": "https://new-url/v1" }
```

All fields are optional. Returns `{ "status": "updated" }`.

**`POST /api/providers/{id}/test`**

```json
{ "status": "connected", "models": [{ "id": "gpt-4", "name": "GPT-4" }] }
```

On failure: `{ "status": "error", "error": "Connection refused" }`.

**`GET /api/providers/{id}/models`**

```json
{
  "models": [
    { "id": "gpt-4", "name": null, "provider_id": null, "context_length": null },
    { "id": "gpt-3.5-turbo", "name": null, "provider_id": null, "context_length": null }
  ]
}
```

---

### MCP Servers

| Method | Endpoint | Description | Returns |
|--------|----------|-------------|---------|
| GET | `/api/mcp/servers` | List all MCP servers | `{ "servers": [...] }` |
| POST | `/api/mcp/servers` | Register a new MCP server | `{ "id": "...", "status": "created" }` |
| POST | `/api/mcp/servers/{id}/connect` | Connect/start an MCP server | `{ "status": "connected" }` |
| GET | `/api/mcp/servers/{id}/tools` | List tools exposed by the server | `{ "tools": [...] }` |
| DELETE | `/api/mcp/servers/{id}` | Disconnect and remove a server | `{ "status": "deleted" }` |

**`POST /api/mcp/servers`**

```json
{
  "name": "Filesystem Server",
  "transport": "stdio",
  "command": "npx",
  "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
  "env": { "NODE_ENV": "production" }
}
```

`transport`: `stdio` | `sse`

For SSE transport (instead of stdio):
```json
{
  "name": "Remote MCP",
  "transport": "sse",
  "sse_url": "https://example.com/mcp"
}
```

Returns:
```json
{ "id": "mcp_a1b2", "status": "created" }
```

**`GET /api/mcp/servers/{id}/tools`**

```json
{
  "tools": [
    {
      "name": "read_file",
      "description": "Read the contents of a file",
      "input_schema": {
        "type": "object",
        "properties": {
          "path": { "type": "string" }
        },
        "required": ["path"]
      },
      "server_id": "mcp_a1b2"
    }
  ]
}
```

---

### Recipes

| Method | Endpoint | Description | Returns |
|--------|----------|-------------|---------|
| GET | `/api/recipes` | List all recipes | `{ "recipes": [...] }` |
| POST | `/api/recipes` | Create a new recipe | `{ "id": "...", "status": "created" }` |
| POST | `/api/recipes/{id}/execute` | Execute a recipe | `{ "session_id": "..." }` |
| DELETE | `/api/recipes/{id}` | Delete a recipe | `{ "status": "deleted" }` |

**`POST /api/recipes`**

```json
{
  "name": "Summarise File",
  "prompt_template": "Please summarise the file at {{ path }} for me.",
  "instructions": "Focus on key findings and action items.",
  "parameters": [
    { "name": "path", "type": "string", "description": "File path", "required": true }
  ],
  "required_mcp_servers": ["filesystem"],
  "system_prompt_layer": "analyst",
  "max_steps": 30
}
```

Returns:
```json
{ "id": "rec_001", "status": "created" }
```

**`POST /api/recipes/{id}/execute`**

```json
{ "parameters": { "path": "/tmp/report.pdf" } }
```

Returns:
```json
{ "session_id": "abc123def456" }
```

Recipe execution:
1. Renders `prompt_template` and `instructions` through Jinja2 with the provided parameters
2. Creates a new chat session tagged with the recipe ID
3. Connects the required MCP servers (if any)
4. Runs the agent with the recipe's system prompt layer as a persona override
5. Returns the session ID for follow-up interaction

---

### Schedules

| Method | Endpoint | Description | Returns |
|--------|----------|-------------|---------|
| GET | `/api/schedules` | List all scheduled jobs | `{ "jobs": [...] }` |
| POST | `/api/schedules` | Create a new scheduled job | `{ "id": "...", "status": "created" }` |
| POST | `/api/schedules/{id}/run_now` | Trigger immediate execution | `{ "status": "triggered" }` |
| PATCH | `/api/schedules/{id}` | Update a scheduled job | `{ "status": "updated" }` |
| DELETE | `/api/schedules/{id}` | Delete a scheduled job | `{ "status": "deleted" }` |

**`POST /api/schedules`**

```json
{
  "name": "Daily Summary",
  "cron": "0 8 * * *",
  "recipe_id": "rec_001",
  "parameters": { "path": "/var/logs/today" },
  "enabled": true
}
```

Returns:
```json
{ "id": "sched_001", "status": "created" }
```

**`PATCH /api/schedules/{id}`**

```json
{ "name": "Renamed Job", "cron": "0 9 * * *", "enabled": false }
```

All fields are optional. Returns `{ "status": "updated" }`.

---

## Database

The daemon uses a local SQLite database at `~/.bigtiny/bigtiny.db` (in-memory for tests).

### Tables

| Table | Purpose |
|-------|---------|
| `sessions` | Chat sessions |
| `messages` | Message history (FK → sessions) |
| `providers` | LLM provider configurations |
| `mcp_servers` | MCP server configurations |
| `recipes` | Recipe definitions |
| `schedule_jobs` | Cron job definitions (FK → recipes) |
| `execution_history` | Scheduled execution logs (FK → sessions) |
| `hitl_rules` | Persistent always-allow/auto-reject rules |

### Schema migrations

Migrations are versioned in a `schema_version` table. Current schema: v003
(v002 added `messages.tool_call_id`; v003 added `messages.content_format` —
`'text'` for plain content, `'blocks'` for a JSON array of multimodal content
blocks such as images).
