# BigTiny backend

Kitty is driven by **BigTiny** (the chat-first REST/SSE daemon, vendored
in-tree at `plugins/bigtiny/`) — its only chat backend. The goosed/ACP
integration this app originally shipped with has been removed entirely; see
`docs/ARCHITECTURE.md` for the current module map.

## Launching it

`bigtiny_command`/`bigtiny_args`/`bigtiny_dir` in `%APPDATA%\Kitty\config.json`
control how the daemon is spawned:

- **Normal installs**: `bigtiny_command` defaults to the bundled
  `bigtiny-daemon.exe` (frozen via `plugins/build.py`, shipped next to
  Kitty's own exe through Tauri's `externalBin`) with empty `bigtiny_args`.
  Nothing to configure — this is fully internalized and never surfaced to
  the user.
- **Dev / source checkout**: if no bundled exe is present (e.g. running via
  `cargo tauri dev`), the config falls back to `bigtiny_command: "python"`,
  `bigtiny_args: ["-m", "bigtiny"]`, and `bigtiny_dir` pointing at
  `plugins/bigtiny` (install its deps once with `pip install -e
  plugins/bigtiny`). The daemon **must** be launched via `python -m bigtiny`
  in this mode: that entry point installs the Windows Proactor event-loop
  factory that stdio MCP servers need to spawn subprocesses (the frozen exe
  doesn't need this — `bigtiny.server.app:loop_factory` handles it directly).

## What the Rust layer does (src-tauri/src/bigtiny/)

- **Lifecycle** (`lifecycle/bigtiny_proc.rs`): free port + random secret per
  launch, passed as `BIGTINY_SECRET`; every request sends it back as
  `X-API-Key`. Also passes `BIGTINY_DATA_DIR` (`config::bigtiny_data_dir()`)
  pointing at `%APPDATA%/Kitty/bigtiny/` — consolidates BigTiny's own db,
  directory-sandbox cache dir, and recipes dir there instead of its
  standalone `~/.bigtiny` default (see `bigtiny/paths.py`); a one-time
  migration moves an existing `~/.bigtiny` over the first time this runs
  post-upgrade. Readiness and the 5s health loop probe `GET /api/health`
  (open without auth by design). A pidfile-based stale-orphan kill (mirrors
  `adaptive_pathway_proc`, now anchored to the same consolidated dir) handles
  the daemon getting orphaned across a `tauri dev` hot-restart.
- **Sessions** (`bigtiny/sessions.rs`): create/list/load/fork/delete over
  REST. `list` translates BigTiny rows into a `sessionId`/`title`/`cwd`/
  `updatedAt` shape the frontend's `parseSession` reads; `load` replays
  history as `chat://user-message` / `chat://message-delta` /
  `chat://tool-call` events; `fork` maps the frontend's "keep the first N UI
  bubbles" index onto BigTiny's inclusive `at_message_id` truncation.
- **Streaming** (`bigtiny/stream.rs`): `POST /api/chat/{id}/send` SSE frames →
  `llm_delta`→`chat://message-delta`, `reasoning_delta`→`chat://reasoning-delta`,
  `tool_start`/`tool_finish`→`chat://tool-call` (also feeds the
  adaptive-pathway `record_outcome` backstop, tool-name-filtered against the
  adaptive-pathway MCP tools themselves), `hitl_pause`→
  `chat://tool-approval-needed` (answered via `POST /approve`; `allow_once`→
  `allow`, `allow_always`→`always_allow`, reject/cancel→`reject`),
  `session_title`→`chat://session-title`, `llm_stop` usage + final frame →
  `chat://complete` with `{stopReason, usage}`.
- **Providers** (`bigtiny/providers.rs`): activating a Kitty provider profile
  registers/updates it in BigTiny over `POST/PATCH /api/providers` — no
  daemon restart needed. `anthropic` maps to BigTiny's native Anthropic
  client; everything else (`ollama`, `openrouter`, `openai`, `custom_openai`)
  maps to `openai_compat` with any trailing `/v1` stripped (BigTiny appends
  `/v1/chat/completions` itself). The API key travels once over localhost and
  lands in BigTiny's own Windows-keyring entry. A provider must always be
  active — BigTiny has no built-in default and errors any send with none
  registered.
- **MCP servers** (`bigtiny/mcp.rs`): list/add/update/delete/connect over
  `/api/mcp/servers`, surfaced in Settings → MCP Servers. Also
  `ensure_builtin_servers`, the self-healing upsert (keyed by name) that
  keeps Kitty's two bundled plugins — `replacement-mcp` and
  `adaptive-pathway` (its `decide`/`record_outcome` tools) — registered
  against the current install's bundled exe path, run on every daemon
  startup and whenever their Settings toggle changes.

## Deliberately different from the old goosed/ACP path

- **No approval modes** (`auto`/`approve`/`smart_approve`): HITL policy is
  enforced daemon-side; `set_mode` is a no-op and sessions advertise no
  modes. The client-side chat/agentic override works as before.
- **No thinking-effort control**: `thinking_effort` is always `null`, so the
  UI hides the dropdown.
- **Recipe extensions are skipped**: BigTiny MCP servers are daemon-global,
  not per-session; recipe prompts still work, their extension hints just
  don't attach anything.
- **Session list carries no provider/model memory** yet — resumed sessions
  stay on the currently-active provider.
- **No context-management-strategy setting** — goosed's `GOOSE_CONTEXT_STRATEGY`
  (summarize/truncate/clear/ask) had no BigTiny equivalent, so the Settings →
  Advanced control for it was removed rather than left silently inert.
- **No MOIM-style prompt nudge** — the file-save-path and adaptive-pathway
  self-call nudges goosed injected via `GOOSE_MOIM_MESSAGE_TEXT` every turn
  have no BigTiny equivalent; the adaptive-pathway `record_outcome` backstop
  in `bigtiny/stream.rs` covers rewards, but not the model proactively
  calling `decide` with real `context`. Backlog item if BigTiny grows a
  system-prompt-injection mechanism.
