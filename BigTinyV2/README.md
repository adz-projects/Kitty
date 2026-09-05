# BigTiny V2

A multi-app LLM orchestration daemon. Several frontends — Kitty, a research
pipeline, an AI notebook — attach to **one** running instance, each with its own
sessions, providers, MCP servers and plugin state, sharing the machine's model
endpoints without starving each other.

```
BigTinyV2/
  daemon/     the daemon itself, forked from plugins/bigtiny_rust/
  protocol/   wire types shared by daemon and clients (no I/O)
  client/     bigtiny2-client: discovery, registration, typed routes
```

## Relationship to V1

`plugins/bigtiny_rust/` (V1) is **frozen** — bug fixes only. It is what Kitty
ships against today and the rollback path for the eventual migration. Every new
feature lands here.

V2 is a **fork, not a rewrite**. V1 is ~29k lines of hard-won correctness — SSE
framing, the directory sandbox, compaction, prompt-prefix determinism, the
hardened MCP transports — and none of that is worth re-learning. Its four
plugin crates (`adaptive-pathway_rust`, `kitty-tools`, `kitty-web`,
`kitty-wasm`) are **not** forked: they are already independent, and V2 takes the
same path dependencies.

V1's 16 migrations are kept intact with `017+` added on top, rather than
squashed. That is what will let the migration open a *copy* of Kitty's real
`bigtiny.db` and move it forward — sessions, history and providers intact —
instead of asking users to start empty.

### The two must never touch each other

Both daemons will run on the same machine throughout the migration.

| | V1 | V2 |
|---|---|---|
| Data dir | `%APPDATA%/Kitty/bigtiny` | `%APPDATA%/BigTinyV2` |
| Env override | `BIGTINY_DATA_DIR` | `BIGTINYV2_DATA_DIR` |
| Binary | `bigtiny-daemon` | `bigtiny2-daemon` |
| Discovery | none (Kitty holds the port) | `daemon.json` handshake |

Separate data dirs are the load-bearing part: sharing one would mean two
daemons opening the same SQLite file with different migration chains, and V2
would migrate the schema out from under V1. The distinct binary name is
belt-and-braces — Kitty's V1 lifecycle kills stale daemons by matching the
process-name fragment `bigtiny-daemon`, which `bigtiny2-daemon` cannot match.

## What is different from V1, so far

### Per-app tenancy (`017_apps_and_tenancy.sql`)

V1 had no client dimension at all. That was coherent while one process spawned
the daemon and owned everything in it; it stops being coherent with a second
frontend, where `GET /api/chat/` returned *every* session and Kitty's provider
activation rewrote every row it did not own.

- `apps` table: registered clients, their hashed keys, and their own default
  provider/model.
- `app_id` on sessions, recipes, schedules and HITL rules (`NOT NULL` — always
  owned by exactly one app).
- Nullable `app_id` on providers and MCP servers, where `NULL` means a shared
  pool visible to every app, so one API key need not be entered per app.
- A `UNIQUE(app_id, name)` index on `mcp_servers`, closing a duplicate-row race
  V1 could only guard with a process-local mutex.

### Identity instead of a shared secret

V1 compared `X-API-Key` against one per-launch secret and answered
allowed-or-401. V2 resolves it to an `AppIdentity` in the request extensions, so
a handler knows *who* is asking and scopes its query. There is no legacy
single-secret mode: V2 is a fork with no existing clients, so nothing needs it.

Keys are long-lived and survive daemon restarts (an app that did not spawn the
daemon has no way to learn a per-launch secret), and only their SHA-256 is
stored — a leaked database yields no usable credentials.

### Per-app MCP servers, recipes and schedules

MCP servers follow the provider model exactly. Recipes and schedules are
`NOT NULL` — they encode one app's workflow, not a machine resource, so there
is no shared variant.

A shared MCP server's `command` is what *every* app's agent loop executes, so
letting one app repoint it would be a code-execution vector against the
others: shared rows are 403 to modify, even for the app that created them.

### Per-app provider resolution

V1 answered "which provider does this caller get?" with a daemon-global sort
over `fallback_priority`. Saying "use mine" therefore meant demoting every
other row to priority 100 — which is exactly what Kitty's
`sync_active_provider` did, and why a second app's choice could not survive the
first app's next activation.

V2 replaces that with `apps.default_provider_id`, a column on the caller's own
row. Resolution order is: an explicit pin → the app's default → the healthiest
provider the app can see. `demote_others` has no equivalent and needs none.

Visibility and mutability are deliberately different questions:

|                        | own private row | shared row (`app_id IS NULL`) | another app's row |
|------------------------|-----------------|-------------------------------|-------------------|
| list / use             | yes             | yes                           | no (404)          |
| patch / delete / probe | yes             | **no (403)**                  | no (404)          |

Shared exists so a user can enter one API key rather than one per frontend.
It is not editable by anyone through the API, including the app that created
it: a row every app routes through is not one client's to repoint.

### Discovery

The daemon publishes `daemon.json` once its listener is bound and migrations
have run; a client validates PID liveness, process name, and a per-launch
`instance_id` echoed on `/api/health` before attaching. A spawn lock ensures
two apps launching together produce one daemon, not two.

The rule that replaces V1's `kill_stale_orphan`: **never kill a daemon that has
not been proven dead.**

### Lifetime: the daemon decides

No client kills the daemon on its own exit — with several attached, none of
them is entitled to. Instead it exits itself after `--idle-exit-mins`
(default 30, `BIGTINYV2_IDLE_EXIT_MINS`, `--no-idle-exit` to stay up) when
**all three** hold:

- no *authenticated* request within the window — health polling deliberately
  does not count, or a client that merely watches the daemon would keep it
  alive forever, and neither does a rejected request, or anyone able to reach
  the port could hold it open with bad keys;
- no turn in flight — a long generation makes no requests while it runs;
- no job or scheduled run active — detached work has no client at all, which
  is exactly why it must not be mistaken for inactivity.

The handshake is withdrawn on the way out, so nobody attaches to a daemon
already leaving.

### Per-app plugins

A **plugin** hooks the agent loop (adaptive pathway: recall before the call,
learning after, a background sweep between). An **MCP server** only provides
tools. The line is loop integration, not statefulness — `kitty-tools` is
stateful and is still only an MCP server. See [PLUGINS.md](PLUGINS.md).

V1 opened exactly one `PathwayEngine` for the whole daemon. V2 opens one per
app, lazily, through `PluginHost`: a single graph shared by several frontends
would mix their beliefs, which is both a privacy leak and a quality regression,
since beliefs blended across two unrelated usage patterns describe nobody in
particular. Graphs live at `apps/<app_id>/pathway.db`.

What stays shared is everything expensive — the embedding model is one loaded
`Arc`, which also keeps every app's vectors in one comparable space.

Turning a plugin off costs *nothing*: no engine, no background sweep, no file.
And it closes a live instance rather than only affecting future lookups.

`GET /api/apps/me/plugins`, `PUT|DELETE /api/apps/me/plugins/{plugin}`.

### Embeddings

`POST /api/embeddings` is served again, from the shared embedder. V1 left it a
permanent 503 after adaptive-pathway stopped calling it over HTTP — but the
model is loaded regardless, and both new apps want vectors for retrieval.
Ollama's `{"prompt": …}` shape is preserved verbatim; `{"input": [...]}` adds
the batch form a pipeline needs.

### Fair scheduling

`ProviderQueue` replaces V1's per-provider semaphore. A semaphore is FIFO —
right for one client, a starvation bug for several: against a one-slot endpoint
an app that queues fifty turns puts fifty entries ahead of the next interactive
message.

- **Round-robin across apps**, so a wait is bounded by the *app count*, not the
  queue depth.
- **Interactive beats background** within an app.
- **Work-conserving** — the fair share binds only while someone else is waiting,
  so one app alone may still use every slot. Without this, the fix for
  cross-app starvation would itself break subagent fan-out.

Endpoint slot counts are **probed** (llama.cpp `/props`) rather than guessed,
falling back to the dialect default. A user-set `parallel_slots` still wins.
`GET /api/providers` reports `concurrency`, `in_flight`, `queue_depth`,
`my_queue_depth` and `slots_source` so a client can pace itself.

Daemon-internal work (compaction, the learn pass) queues in a reserved
`__daemon__` lane: `SummarizerChain` implements a trait whose signature carries
no identity, so it genuinely cannot name the app it serves.

### Detached jobs and resumable streams

`POST /api/jobs` submits a turn with no stream attached, so work survives its
submitter going away. Jobs left `running` by a previous process become
`interrupted` at boot — never re-queued, because a turn may have executed tools
with side effects and silently repeating them is worse than stopping.

`GET /api/chat/{id}/stream` rejoins a turn already in progress, resuming from
`Last-Event-ID`. **This reverses an earlier decision** to skip SSE fan-out: that
reasoning was about two clients *starting* work, where the per-session 409 is
still correct, and says nothing about one client rejoining running work. A
second send still 409s.

Structural events are buffered unconditionally; text deltas stay best-effort,
and a resuming client is *told* when text was lost rather than handed a silently
incomplete transcript.

### Structured output

Per-dialect, because none of them agree: OpenAI takes `response_format`,
Anthropic forces a single tool whose `input_schema` is the schema (tool-forcing
*is* its mechanism, not a workaround), and self-hosted servers take the schema
in `format`. The tool loop runs normally and the schema constrains only the
final answer, so a turn can call tools and still return validated JSON.

### Response cache

Content-addressed, keyed on everything that can change a response. **Per-app by
default**: serving app Y a response derived from app X's prompt would be a
silent leak, so the app id is in the key rather than in a filter, and sharing is
an explicit per-request opt-in. Never caches a turn that ran tools, an error, or
a partial response. A hit acquires no queue permit — a hit that queued behind
live traffic would save the tokens but not the latency.

Distinct from the `cache` config, which is about prompt-*prefix* determinism for
KV reuse. That makes the same call cheaper; this makes a repeat call free.

### Cross-session search

`GET /api/search` exposes the FTS5 index that has existed since migration 009
but was only ever reachable from the internal per-session recall path. Scoped
by joining through `sessions.app_id` **in the SQL**, not filtered afterwards:
this route returns raw conversation text, so a leak here hands one app the
contents of another's chats rather than merely confirming an id exists.

## Tests

```bash
cd BigTinyV2/protocol && cargo test
cd BigTinyV2/client   && cargo test
cd BigTinyV2/daemon   && cargo test
```

`daemon/tests/tenancy.rs` is the guard the tenancy design rests on. Session
routes call `deny_unless_owned` at entry rather than threading `app_id` through
the whole crate — much less invasive, but it means a *new* route could forget
the check. That file sweeps every `/api/chat/{id}/*` route as a non-owner and
requires a 404 from each, so the mistake fails there instead of leaking someone
else's conversation.

**Add a row to its `session_routes` list whenever you add a session route.** A
route missing from that list is a route nobody proved is scoped.
