# BigTiny V2 — wire contract

`api_version: 1`. Every route is under `/api`.

The daemon serves several apps at once. Two rules run through everything below:

- **You only ever see your own things.** Another app's session, job, specialist or
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

## Specialists

A **specialist** is a named delegate agent the model can call mid-turn through
the built-in `call_specialist` tool. It runs in its own session, with its own
model pin, its own tool allow-list, and a required JSON answer shape — so its
tool calls never enter the calling session's transcript and the caller sees only
the result.

| Route | Notes |
|---|---|
| `GET /api/specialists` | your own, plus the built-ins you have not shadowed |
| `POST /api/specialists` | `{name, description, system_prompt, provider?, model?, tool_allow?, response_schema?, max_steps?, enabled?}` to `{id}`; also the edit path |
| `DELETE /api/specialists/{id}` | your own only; **403** on a built-in |
| `POST /api/specialists/{name}/run` | `{request, refs?, session_id}` to `{ok, ran_on, notes, result}` — runs to completion |
| `GET /api/specialists/runs` | the last 100 delegate runs, for diagnosing what routed where |

`description` is the only text the calling model reads when deciding whether to
delegate, so a vague one does not fail loudly; it just gets used for the wrong
things.

`tool_allow` is enforced at dispatch, not merely used to shape what the delegate
is offered — a model calling a tool it was never shown is refused. A name no
connected server provides is a **400** at write time rather than a surprise at
run time. An empty array means *no* tools, never "unrestricted".

Built-ins (`researcher`, `summarizer`, `locator`, `extractor`, `analyst`) are
seeded by the daemon and shared by every app: **403** to modify or delete.
POSTing one's name creates your own definition that shadows it for you alone;
deleting that reverts to the built-in.

Delegates run at background priority, cannot start delegates of their own, are
capped by `agent.max_concurrent_specialists`, and are cancelled when their parent
session is or when they exceed `agent.specialist_timeout_secs`. A run that needs
human approval refuses immediately and says so in its answer rather than waiting
for an approver that does not exist.

**Host selection.** A delegate does not simply run wherever the parent did. The
daemon scores every provider visible to the app — tool support and health are
hard gates, then whether it shares a single KV slot with the parent, its
concurrency, and (when Kitty supplies them) `cost_tier` and `capability_rank`
from its own provider config. A specialist's `provider`/`model` pin wins if it is
still eligible; if it is not, **both** are dropped together, because a model
chosen for one provider is not meaningful at another. The result names what it
actually ran on in `ran_on`, and any degradation in `notes`.

`agent.subagent_model_deny` (exact ids or `prefix*`) is checked at final
resolution, including the step that would otherwise fall back to the parent's own
model — so a denied model is refused rather than used as a last resort, and the
refusal names the pattern. The list is also copied into the delegate's own
session metadata, because host selection is not the last word on which model runs:
the turn loop re-resolves the provider at step 0 and again on every failover, and
neither knows a specialist is asking. A delegate placed on a permitted host will
decline a failover onto a denied one and stay where it is.

**Reasoning budget.** Each delegate gets one, from its own `reasoning_cap` or
`agent.specialist_reasoning_fraction`. Enforced by a per-run accumulator in the
agent loop rather than on the wire, because only Anthropic and OpenRouter accept
a budget field — the other dialects would silently ignore one. Exceeding it
disables thinking for the rest of the run and tells the model to finish; it never
aborts, because the delegate still owes its caller a report.

**Fan-out.** A specialist with `fan_out: "per_ref"` (built-in `extractor` and
`summarizer`) turns one call carrying N refs into N delegates, returning
`{succeeded, failed, results[]}`. A failure is per element, never for the batch.
Refs are capped at 32, and the whole batch shares one deadline of three times
`specialist_timeout_secs` — a per-run ceiling bounds one child, and thirty-two
children against three permits is eleven waves of it with the parent's tool call
blocked throughout. A source the batch did not reach is reported as such, so a
caller can re-run just the ones that are missing.

Progress reaches a watching client as `subagent_status` frames carrying the
*child's* session id, so its full transcript stays readable through
`GET /api/chat/{child}/history` when the report was not enough.

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
