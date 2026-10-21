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
| `PATCH /api/chat/{id}/config` | shallow-merges metadata; see "Reasoning effort" below |
| `GET /api/chat/{id}/history` | bare array |
| `POST /api/chat/{id}/send` | SSE stream, below |
| `GET /api/chat/{id}/stream` | rejoin a turn in progress |
| `POST /api/chat/{id}/fork` | the fork belongs to the forking app |
| `POST /api/chat/{id}/cancel`, `/compact`, `/approve` | |
| `GET /api/chat/{id}/stats`, `/timings`, `/pending` | |

**Reasoning effort.** A session's `thinking_effort` metadata key (a string,
set through `PATCH /api/chat/{id}/config`) is translated per provider dialect
on every turn. `""` or absent: no effort sent, the model's own default.
`off` or `none`: reasoning off. `on`: reasoning on with no grade (toggle-only
models). Anything else (`minimal`, `low`, `medium`, `high`, `xhigh`, `max`,
or a self-hosted template's own level) is sent **verbatim**: the caller is
expected to send only levels it detected for the model.

| Dialect (`provider_dialect`) | Graded level | Off | On |
|---|---|---|---|
| `openai`, `fireworks`, `deepinfra` | `reasoning_effort: <level>` | `reasoning_effort: "none"` | nothing |
| `openrouter` | `reasoning: {effort: <level>}` (or `{max_tokens}` under a reasoning cap) | `reasoning: {enabled: false}` | `reasoning: {enabled: true}` |
| `custom_openai`, `ollama` | `chat_template_kwargs: {enable_thinking: true, reasoning_effort}` + top-level `reasoning_effort` | `enable_thinking: false` | `enable_thinking: true` |
| `anthropic`, provider `config.anthropic_thinking: "adaptive"` | `thinking: {type: "adaptive"}` + `output_config: {effort}` | `thinking: {type: "disabled"}` | `thinking: {type: "adaptive"}` |
| `anthropic`, `anthropic_thinking` absent or `"budget"` | `thinking: {type: "enabled", budget_tokens}` (4096 / 16384 / 32768) | no `thinking` | top budget |

A 400 that rejects the effort control is reported as a `provider_error` with
`error_type: "effort_rejected"` and is not retried.

**Delegate effort** (2.3.0). A provider's `config.delegate_effort` is the level
a delegate hosted on it runs at — a level that model accepts, never `off`
(Kitty sends `low`, else the ladder's lowest grade, else `on`). It becomes the
delegate session's `thinking_effort`, the level its reasoning budget drops to
when spent, and the level of its schema-constrained final pass. Absent: the
delegate sends no effort and runs at the model's own default.

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
default (`slots_source` says which). The default is 1 for a self-hosted
`custom_openai`/`ollama` endpoint and for `local`, 8 otherwise. "Self-hosted"
is the provider's `config.hosted: false`, or, when `hosted` is absent, a base
URL on loopback, RFC 1918, link-local, unique-local or Tailscale
(`100.64.0.0/10`, `*.ts.net`), or a `.local`/`.lan`/`.internal`/single-label
name (2.3.0 — before it, every `custom_openai` endpoint got 1, including hosted
APIs saved under that dialect). Pace against it rather than
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
seeded by the daemon and shared by every app: **403** to modify or delete. A
built-in row stored from an older definition (`builtin_version` below the
engine's) is refreshed at start-up, keeping its `enabled` flag (2.3.0; before
it, an install kept its first definitions forever).
POSTing one's name creates your own definition that shadows it for you alone;
deleting that reverts to the built-in.

Delegates run at background priority, cannot start delegates of their own, are
capped by `agent.max_concurrent_specialists`, and are cancelled when their parent
session is or when they exceed `agent.specialist_timeout_secs`. A run that needs
human approval refuses immediately and says so in its answer rather than waiting
for an approver that does not exist.

**Host selection.** A delegate does not simply run wherever the parent did. The
daemon scores every provider visible to the app — tool support and health are
hard gates, then whether it shares a single KV slot with the parent (a hosted
endpoint never does, whatever its slot count), its concurrency (a hosted
endpoint's default counts; a self-hosted guess does not), and (when Kitty supplies them) `cost_tier` and `capability_rank`
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
drops the rest of the run to the host's `delegate_effort` (2.3.0; it used to
drop the effort entirely, which on most models meant their own, higher default)
and tells the model to finish; it never aborts, because the delegate still owes
its caller a report.

**The final answer.** After the tool loop, a schema'd delegate is asked once
more, with tools withheld and the schema attached, at its `delegate_effort` and
with at least 8192 tokens of room (bounded by the window). The schema reaches
the wire as `response_format: json_schema` (OpenAI, OpenRouter and the other
hosted dialects), `format` (llama.cpp, Ollama), `output_config.format: {type:
"json_schema", schema}` on an Anthropic provider with `anthropic_thinking:
"adaptive"` (2.3.0 — current Claude models reject a forced `tool_choice`; the
schema sent there is closed with `additionalProperties: false` and stripped of
the constraints structured outputs reject, and the answer is still validated
against the full schema), or a forced tool on older Anthropic models. One retry
follows a rejected shape, a truncated answer (with twice the room) or a
transport error. If both passes fail, the delegate's last prose comes back on
the failure as `How far it got: …`, as it does after a timeout.

**Per-session switch.** A session whose metadata has `specialists_disabled:
true` (set through `PATCH /api/chat/{id}/config`) is never offered
`call_specialist`, `await_specialists` or `list_specialists` — they are left
out of its tool list, not offered and refused — and a call to one anyway is
refused at dispatch. Absent or `false`: specialists are on. A fork copies the
session's metadata, so it inherits the switch (2.3.0).

**When the model delegates.** `call_specialist`'s description states the rules
as rules: delegate only self-contained work that needs many tool calls and can
run while the model does something else; never a single lookup, file read or
command, anything answerable from the conversation, anything finishable in
about three tool calls, or a follow-up to the user's last message; when unsure,
do it directly; batch independent pieces into one call (2.3.0).

Delegate sessions are marked `delegate: true` in their metadata and skip the
memory hooks (pathway, memorabilia and pre-flight recall) and the post-turn
passes (compaction, pathway learn, memorabilia harvest): their "user" is
another model, and nobody continues their transcript.

The wrap-up reserve before a delegate's deadline is `min(120 s, 2/5 of its
budget)`, enough for a wrap-up reply and both final passes.

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

## Decision model

An app may point the engine at a **decision model**: a small classifier that
answers typed questions (yes/no, pick one, rate) over a piece of text in one
forward pass — Liquid's d1 served by `llama-server` today. The engine uses it,
per app, to make things it already does cheaper or sharper; every use fails
open, so a missing, slow or unreachable model leaves behaviour exactly as it
was. See `daemon/src/decider/` and `prompt_determinism.md`.

`GET /api/apps/me/decider` → `{config | null, dialects[], default_thresholds}`.

`PUT /api/apps/me/decider` replaces the app's configuration (400 on an unknown
dialect, a bad URL or duplicate group names); `DELETE` removes it.

```json
{
  "dialect": "systemone",
  "url": "http://127.0.0.1:8080",
  "model": "d1-3B-Q8_0.gguf",
  "features": {"tool_gating": true, "learn_gate": true, "recall_rerank": true,
               "ingest_gate": true, "page_classify": true, "search_rerank": true,
               "compaction": true, "shell_screen": true},
  "thresholds": {"tool_gating_add": 0.6},
  "tool_groups": [{"name": "office", "description": "reading Word, Excel or PDF",
                   "tools": ["lean_word_*", "lean_pdf_*"], "always": false,
                   "file_types": [".docx", ".pdf"]}],
  "cache_ttl_secs": 300
}
```

A missing feature is off. `thresholds` needs only the overrides; the rest take
the defaults. `tool_groups` is the app's own catalog (a trailing `*` matches a
prefix); a tool in no group is never gated. A group's `file_types` load it
outright when a file of that type is attached (the message's `Files provided by
the user:` list, or on a chat's first turn its `attached_paths`). `page_classify` and
`search_rerank` are read by the app's web tools, not the engine.

`GET /api/apps/me/decider/stats` → `{features: {name: {calls, answered,
failed, acted}}}` since the engine started (`acted` = an answer changed what
the engine did).

`POST /api/apps/me/decider/probe` `{dialect?, url}` → `{ok, model, rtt_ms,
error}`, without saving anything.

`POST /api/apps/me/decider/screen-command` `{command}` → `{risk, flagged}` (the
`shell_screen` feature), or `{risk: null, flagged: false}` when it is off or had
no answer. **Escalation only**: an app holds a flagged command for a person and
decides anything else exactly as it would have.

**Tool gating** keeps a chat's tool set in its metadata (`tool_gate`) and adds
a loop-only tool, `load_tool_group`, while gating is on. Usage entries in
`metadata.usage[]` carry `cache_read_tokens` / `cache_creation_tokens` when the
provider reports them.

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
