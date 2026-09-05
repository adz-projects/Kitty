# BigTiny V2 — wire contract

`api_version: 1`. Every route is under `/api`.

The daemon serves several apps at once. Two rules run through everything below:

- **You only ever see your own things.** Another app's session, job, recipe or
  private provider answers **404**, never 403 — "this exists but is not yours"
  is itself a disclosure.
- **Visibility and mutability are different questions.** You may *use* a shared
  provider or MCP server; you may not reconfigure one. That is **403**.

## Authentication

`X-API-Key: <key>` on every route except `/api/health`.

Keys are issued by registration, survive daemon restarts, and are stored only
as a SHA-256 hash — a leaked database yields no usable credentials, and a lost
key cannot be recovered from the daemon.

### Getting a key

1. Read the handshake at `%APPDATA%/BigTinyV2/daemon.json` (`~/.bigtiny-v2/`
   elsewhere).
2. `POST /api/apps/register` with `X-Registration-Token: <registration_token>`
   from that file, body `{"app_id", "display_name"}`.
3. Store the returned `api_key` in your own secret store. Registering again
   with the same `app_id` is **409**, never a reissue — otherwise anyone who
   can read the handshake could take over an existing app's identity.

The registration token is per-launch and authorizes registration only. The
handshake file is readable by any process running as the user, so this is a
boundary against *accidental* cross-app interference, not against local
malware: anything that could read the file could read your issued key too.

## Discovery

`GET /api/health` — unauthenticated, so a client can poll readiness and
validate a handshake before it has a key.

```json
{"status":"ok","instance_id":"<per-launch>","api_version":1,"local":{}}
```

Before attaching, check all three: the handshake's `pid` is alive, its process
name contains `bigtiny2-daemon`, and `instance_id` here matches the file's. A
live PID on the recorded port is *not* proof — the daemon may have restarted
and been handed the same port, in which case its registration token and
database generation differ from the file you read.

Attaching to a daemon older than your `api_version` should be a clear error.
Never a silently wrong-shaped call.

## Sessions

| Route | Notes |
|---|---|
| `POST /api/chat/` | `{cwd?, mode?, name?, provider?, model?}` to `{session_id}` |
| `GET /api/chat/` | `?limit&offset`; `total` is scoped to you |
| `PATCH /api/chat/{id}` | rename |
| `DELETE /api/chat/{id}` | 404 if it matched nothing |
| `PATCH /api/chat/{id}/config` | shallow-merges metadata |
| `GET /api/chat/{id}/history` | bare array |
| `POST /api/chat/{id}/send` | SSE stream, below |
| `GET /api/chat/{id}/stream` | rejoin a turn in progress |
| `POST /api/chat/{id}/fork` | the fork belongs to the forking app |
| `POST /api/chat/{id}/cancel`, `/compact`, `/approve` | |
| `GET /api/chat/{id}/stats`, `/timings`, `/pending` | |

**One turn per session.** A second concurrent `POST /send` is **409**. To run
work in parallel, use several sessions — that is what `parent_session_id`
groups. Rejoining a running turn is `GET /{id}/stream`, which is a different
thing from starting one.

## Streaming

`data: {json}` terminated by a blank line, optionally preceded by `id: <n>`.

15 event types: `llm_delta`, `reasoning_delta`, `llm_stop`, `tool_start`,
`tool_finish`, `hitl_pause`, `hitl_resolved`, `error`, `model_failover`,
`subagent_status`, `session_status`, `session_title`, `compaction`,
`provider_error`, `llm_timing`.

`is_last: true` marks a terminal frame. Note it is **omitted when false**, as
are all defaulted fields — deserialize with defaults, or the daemon's own
output will not parse. (`recoverable` defaults to **true** when absent.)

### Resuming

`GET /api/chat/{id}/stream` with `Last-Event-ID: <n>` replays everything after
`n`, then follows the live turn. Without the header you get the whole retained
buffer.

**Only structural events carry ids and are replayable.** Text deltas are
best-effort: the send path drops them under queue pressure rather than stalling
the turn, so they were never recoverable. A resume that lost text is announced
with a `session_status` of `ResumedWithGap` before the replay — a gap you know
about can be repaired by reading `/history`; one you do not know about cannot.

## Jobs

Detached work: a turn with no stream attached, so it survives your process
exiting.

| Route | Notes |
|---|---|
| `POST /api/jobs` | `{prompt, session_id?, parent_session_id?, provider?, model?, name?}` to `{job_id, session_id}` |
| `GET /api/jobs` | `?status&limit` |
| `GET /api/jobs/{id}` | |
| `DELETE /api/jobs/{id}` | cancel; **409** if already terminal |

States: `pending`, `running`, then `succeeded` / `failed` / `cancelled`, plus
`interrupted`.

**`interrupted` means the daemon stopped mid-job and it was *not* re-queued.**
A turn may have executed tools with side effects, and silently re-running it
would repeat them. Resubmitting is your decision.

Jobs queue at background priority, so a batch never delays another app's
interactive chat.

## Providers

`GET /api/providers` returns your own plus the shared pool, each with live
queue state:

```json
{"id":"p1","name":"local","has_api_key":true,
 "queue":{"concurrency":4,"in_flight":2,"queue_depth":7,
          "my_queue_depth":3,"slots_source":"probed"}}
```

`concurrency` is what the endpoint actually serves at once — your configured
`parallel_slots` if set, else probed from llama.cpp's `/props`, else the
dialect default (`slots_source` says which). Pace against it rather than
discovering the limit as latency. `my_queue_depth` distinguishes "the endpoint
is busy" from "*I* have a backlog".

API keys are never echoed back; `has_api_key` is a boolean.

`POST /api/providers` takes `"shared": true` to put a provider in the pool
every app can see. Shared rows are **403 to modify, including for their
creator** — a row every app routes through is not one client's to repoint.

`PATCH /api/providers/{id}`, `DELETE`, `POST /{id}/test`, `GET /{id}/models`.

## MCP servers

Same model: `GET|POST /api/mcp/servers`, `PATCH|DELETE /{id}`,
`POST /{id}/connect`, `GET /{id}/tools`. `"shared": true` on create.

A shared server's `command` is what *every* app's agent loop executes, so
shared rows are 403 to modify or connect.

## Plugins

A **plugin** hooks the agent loop and gets a per-app instance; an **MCP
server** provides tools and gets per-app selection. Different things, different
routes — see `PLUGINS.md`.

`GET /api/apps/me/plugins` reports the *effective* state plus `explicit`, which
distinguishes a choice from an inherited default.

`PUT /api/apps/me/plugins/{plugin}` `{enabled}`; `DELETE` returns to the daemon
default. Disabling closes any live instance, so it costs nothing rather than
merely hiding tools.

## Embeddings

`POST /api/embeddings`, served from the daemon's in-process model.

- `{"prompt": "text"}` returns `{"embedding": [...], "model", "dims"}`
  (Ollama-shaped)
- `{"input": ["a", "b"]}` returns `{"embeddings": [[...]], "model", "dims"}`

Batch up to 256. A failed row fails the whole request rather than returning a
short array — a silently misaligned index is worse than an error. **503** means
no embedding model is configured, which is distinguishable from a wrong URL.

## Search

`GET /api/search?q=&session_id=&limit=` — full-text across your own history.
Scoped by joining through session ownership in the query itself.

## Apps

`GET /api/apps` (no secrets), `GET|PATCH /api/apps/me`
(`{default_provider_id, default_model}`), `DELETE /api/apps/{id}` — self only;
cross-app revocation would be a denial of service.

**Provider selection is per app.** An explicit pin wins, then your
`default_provider_id`, then the healthiest provider you can see. There is no
daemon-global "active provider" to fight over.

## Errors

`{"error": "..."}` with:

| Code | Meaning |
|---|---|
| 400 | malformed request |
| 401 | missing or invalid key |
| 403 | visible but not yours to modify (shared rows) |
| 404 | does not exist, **or is not yours** |
| 409 | conflict: turn in progress, app id taken, job already terminal |
| 502 | provider reachable but failing |
| 503 | capability not configured on this daemon |
