# ACP protocol reference (Goose 1.41.0)

Vendored/pinned reference for the Agent Client Protocol surface this app targets,
confirmed live against `goose serve` 1.41.0 on 2026-07-04. All path/method
assumptions in `src-tauri/src/goosed/` derive from this file. Re-verify on a
Goose version bump (see [VERSIONS.md](VERSIONS.md)).

## Transport

- **WebSocket**, JSON-RPC 2.0, one bidirectional connection multiplexing sessions.
- URL: `ws://127.0.0.1:<port>/acp?token=<secret>` (`wss:` under TLS). The secret
  (`GOOSE_SERVER__SECRET_KEY`) is passed as the `token` query param; the desktop
  also sends it as the `X-Secret-Key` header on the upgrade request.
- HTTP readiness (not ACP): `GET /status` and `GET /health` with header
  `X-Secret-Key: <secret>`. (Startup readiness check is "GET /status".)

## Handshake (client → agent requests)

1. `initialize` → `{ protocolVersion: 1, clientCapabilities: { fs: { readTextFile, writeTextFile } } }`
   - result: `{ protocolVersion, agentCapabilities: { loadSession, promptCapabilities, sessionCapabilities, ... }, authMethods, _meta }`
2. `authenticate` `{ methodId }` — only if `authMethods` requires it (local Ollama does not).
3. `session/new` `{ cwd, mcpServers: [] }`
   - result: `{ sessionId, modes: { currentModeId, availableModes: [{ id, name, description }] } }`
   - modes observed: `auto` (auto-approve), `approve` (ask every tool), `smart_approve`.
   - `sessionId` example: `"20260704_1"`.
4. `session/load` `{ sessionId, cwd, mcpServers }` — resume. Replays the whole
   conversation as ordered `session/update` notifications
   (`user_message_chunk`, `agent_thought_chunk`, `tool_call`/`tool_call_update`,
   `agent_message_chunk`), then returns `{ modes }`. (Phase 4)
   - `session/list` → `{ sessions: [{ sessionId, cwd, title, updatedAt, _meta: { messageCount, createdAt, lastMessageAt, providerId, modelId, sessionType } }] }`.
   - `session/delete` `{ sessionId }` (extension method).
  - `session/fork` `{ sessionId, cwd }` → `{ sessionId: <new>, modes }` (full copy;
    unknown params are silently ignored). Phase 9 branch = fork **then**
    `_goose/unstable/session/conversation/truncate` `{ sessionId, truncateFrom }`
    (keeps messages before `truncateFrom`).
   - Working-dir change on an existing session is the *unstable* extension method
     `_goose/unstable/session/working-dir/update` — we avoid it; "Set as working
     directory" starts a new `session/new` rooted at the folder instead.
5. `session/prompt` `{ sessionId, prompt: [ContentBlock] }`
   - streams `session/update` notifications, then result: `{ stopReason, usage: { totalTokens, inputTokens, outputTokens } }`
   - `stopReason`: `end_turn | max_tokens | max_turn_requests | refusal | cancelled`.
6. `session/cancel` `{ sessionId }` — notification (Phase 8).

`ContentBlock`: `{ type: "text", text }` (also image/audio per promptCapabilities).

### Image content blocks — probed shape (Round-3 Batch 8, 2026-07-05)

- `initialize`'s result reports `agentCapabilities.promptCapabilities: { image: true,
  audio: false, embeddedContext: true }` for the pinned Goose version — no
  separate negotiation step needed before sending an image block.
- **Confirmed working shape**: `{ type: "image", data: "<base64, no data: prefix>",
  mimeType: "image/png" }` — camelCase `mimeType` (not `mime_type`), and `data`
  is a top-level field (not nested under an Anthropic-style `source` object).
  A real `session/prompt` call with this shape alongside a text block returned
  `{ stopReason: "end_turn", usage: {...} }` successfully.
- Rejected shapes (schema errors, not capability errors): `mime_type` (snake_case)
  → `missing field \`mimeType\``; Anthropic-style `{ source: { type: "base64",
  media_type, data } }` → `missing field \`data\`` (goosed wants `data` directly
  on the block, not nested).
- This is an agent-level capability flag (reported once at `initialize`, not
  per-provider/model) — whether the underlying model can actually *see* the
  image is a separate, model-specific question this app can't detect in
  advance; sending the block and letting a non-vision model degrade to a
  text-only-informed answer is strictly better than today's hard path-based
  failure.

## Streaming (agent → client `session/update` notifications)

Shape: `{ sessionId, update: { sessionUpdate: <variant>, ... } }`. Variants seen:

| variant | payload | maps to |
|---|---|---|
| `agent_message_chunk` | `{ content: { type:"text", text } }` | visible answer delta → `chat://message-delta` |
| `agent_thought_chunk` | `{ content: { type:"text", text } }` | reasoning delta → `chat://reasoning-delta` (Phase 10) |
| `tool_call` | `{ toolCallId, title, rawInput, _meta.goose.toolCall.{toolName,extensionName} }` | `chat://tool-call` (new) |
| `tool_call_update` | `{ toolCallId, status, content:[{type:"content",content:{type:"text",text}}], rawOutput:{stdout,stderr,exit_code} }` | `chat://tool-call` (update) |

> Tool-call frames confirmed live (shell tool): the initial `tool_call` has the
> title + `rawInput` but **no status**; the completion `tool_call_update` carries
> `status:"completed"` + `rawOutput`; a trailing update may set a new `title`
> ("the task completed") and omit status. Client keeps the first title and applies
> status/output only when present.
| `session_info_update` | `{ title, updatedAt, _meta }` | session title → `chat://session-title` |
| `usage_update` | `{ used, size }` | token context usage |
| `available_commands_update` | `{ availableCommands: [...] }` | slash commands (later) |
| `current_mode_update` | `{ currentModeId }` | approval-mode badge (Phase 3) |
| `plan` | plan entries | (later) |

## Agent → client requests (we must respond)

- `session/request_permission` `{ sessionId, toolCall: { toolCallId, title, rawInput, kind, status }, options: [{ optionId, name, kind }] }`
  → respond `{ outcome: { outcome: "selected", optionId } }` or `{ outcome: { outcome: "cancelled" } }`.
  Options confirmed live (approve mode): `allow_always`, `allow_once`, `reject_once`,
  `reject_always`. Only sent in `approve` / `smart_approve` modes — in `auto`
  mode (default) it is never sent. **Phase 3** defers these (stores the JSON-RPC
  id keyed by `toolCallId`) and responds only on the user's Approve/Deny.

## Mode switching (Phase 3)

- `session/set_mode` `{ sessionId, modeId }` → `{}`. `modeId` ∈ `auto` /
  `approve` / `smart_approve` (from `session/new` result `modes.availableModes`).
- Live `current_mode_update` `session/update` reflects out-of-band changes.

## Config surface (Phase 5)

- **No clean ACP method to set the global provider/model.** Config is almost all
  `_goose/unstable/*` extension methods; `session/set_model` changes the model
  per session, but there is no provider-config write.
- **Approach:** we route goosed to a provider by injecting Goose's env when we
  spawn `goose serve` — `GOOSE_PROVIDER`, `GOOSE_MODEL`, and provider keys
  (`OPENROUTER_API_KEY` / `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` + `OPENAI_BASE_URL`
  / `OLLAMA_HOST`), plus `GOOSE_TEMPERATURE` / `GOOSE_CONTEXT_LIMIT`. Activating a
  profile persists it and restarts goosed. Secrets come from the keyring.
- **Extensions:** session-scoped `_goose/unstable/session/extensions/list|add|remove`.
- **Sessions:** `session/list`, `session/load`, `session/delete` (all confirmed).

### Extensions — probed shapes (Round-2 Batch 0, 2026-07-05)

- `_goose/unstable/session/extensions/list` `{ sessionId }` → `{ extensions: [ … ] }`.
  Entry shape is **tagged by `type`** and differs per type:
  - `builtin` / `platform`: `{ type, name, description, display_name, bundled, timeout? }`
  - `mcp`: `{ type: "mcp", server: { name, command, args, env }, envKeys: [ … ], description, timeout }`
    — **⚠ mcp entries have NO top-level `name`; the identity lives at `server.name`.**
    This is the source of the "blank checkbox" bug (Round-2 item 23): the frontend
    reads `e.name`, which is `undefined` for every mcp extension. Fallback must be
    `e.name ?? e.display_name ?? e.server?.name ?? '(unnamed)'`.
  - The list response carries **no per-entry `enabled` flag** — enable/disable state
    is not returned here (frontend currently assumes `enabled !== false` = on).
- `_goose/unstable/session/extensions/add` `{ sessionId, extension: <Extension> }`.
  `<Extension>` is the same tagged enum: `type` ∈ { `builtin`, `platform`, `mcp` }
  (NOT `stdio`/`sse`). `{ type: "builtin", name: "memory" }` succeeds. A **custom
  stdio/HTTP server** is added as `type: "mcp"` with a `server` object
  `{ name, command, args, env }` (mirrors the list shape). Missing `type` →
  `missing field \`type\``; bad type → `unknown variant …, expected one of builtin, platform, mcp`.
- **Web-search capability** (Round-2 item 14a): the bundled search extension is
  `mcp-brave-search` (MCP: `npx -y @modelcontextprotocol/server-brave-search`,
  requires a `BRAVE_API_KEY` env). The `computercontroller` builtin also provides
  general web/computer tools without a key. So "web search in every mode" either
  enables `mcp-brave-search` (needs the user's key) or leans on `computercontroller`.
- **Custom mcp extension `server.env` shape confirmed as an array** (Round-3
  item 14 probe): `{name, command, args: [...], env: [...]}` — a bare string
  array (e.g. `["KEY=VALUE"]`), matching the `mcp-brave-search` example's
  `"env": []`, not a key-value map. The shape validates and goosed attempts to
  spawn the process; a bogus one-shot command (e.g. `echo`, or `node --version`)
  fails at the process level (`IO error: program not found` / `process quit
  before initialization`) rather than at JSON-schema validation — confirming the
  shape itself is correct (a real long-running MCP stdio server is needed for a
  full end-to-end success, not tested here).

### Recipes / skills — NOT an ACP session param (Round-2 Batch 0)

- `session/new` **silently ignores unknown fields** — probing it with `recipe` /
  `recipePath` / `instructions` / `systemPrompt` all returned OK, but that only
  means they were dropped, not honored. There is **no ACP method to launch a recipe**.
- Goose recipes are **file-based YAML**, run via the CLI: `goose run --recipe
  <name|full-path> [--params KEY=VALUE] [--sub-recipe …]`, plus `goose recipe
  {list,validate,deeplink,open}`. `goose skills {list}` is a **separate** subcommand.
  → Round-2 item 16 ("recipes") is **Path B** (a separate `goose run --recipe`
  process outside the shared `goose serve`) — an architecture escalation to surface
  to the user before building (see Round-2 plan Batch 7).
- **Resolved**: rather than take on Path B, recipes shipped as client-side-
  interpreted templates that attach to an ordinary chat turn instead of shelling
  out to the real CLI runner — see `docs/BACKLOG.md`'s "Recipes (resolved)" entry
  and `chatStore.ts`'s `sendWithRecipe`. This finding above stays accurate as the
  reason why that design was chosen over a literal `goose run --recipe` launch.
- `fs/read_text_file`, `fs/write_text_file` — only if we advertise the capability
  (we set both `false` for now), so respond method-not-found otherwise.

### Extension defaults + reasoning effort — live goose config file (Round-7 probe, 2026-07-10)

- **Confirmed**: `_goose/unstable/session/extensions/list` on a *brand-new* session
  (before any Kitty `.../add` call except the hardcoded `computercontroller`) already
  returns every extension marked `enabled: true` in goose's own persistent
  `config.yaml` — bundled platform extensions (`developer`, `skills`, `todo`, `tom`,
  `chatrecall`, `summon`) *and* the user's own configured custom MCP servers
  (`mcp-brave-search`, and any other `stdio`-type entry with `enabled: true`).
  Extensions marked `enabled: false` in that file (confirmed: `code_execution`,
  `extensionmanager`, `summarize`, `analyze`, `orchestrator`, `apps`,
  `autovisualiser`, `memory`, `tutorial`, `localrag` on the probing machine) do
  **not** appear in the list at all. So `session/new` auto-attaches whatever's
  `enabled: true` in this file — it's the real "default extensions for new chats"
  surface, not something session-scoped ACP calls can see beyond reflecting it.
- **The file itself**: Windows path `%APPDATA%\Block\goose\config\config.yaml`
  (i.e. `dirs::config_dir()` + `Block/goose/config/config.yaml`). Top-level YAML
  map; the `extensions:` key is itself a map keyed by extension id, each entry
  `{ enabled, type, name, description, display_name, bundled, timeout?, cmd?,
  args?, envs?, env_keys?, cwd? }` (the `stdio`-type shape here, e.g.
  `mcp-brave-search`/custom user extensions, differs a little from the ACP
  `mcp`-type shape documented above — notably `cmd`/`envs`/`env_keys` instead of
  `server: {command, env}`/`envKeys`, and `type: stdio` not `type: mcp`). This is
  goose's own config file, shared with Goose Desktop and any other local goose
  usage — editing it is a real cross-app surface, not something private to Kitty.
- **`GOOSE_THINKING_EFFORT`** appears as a top-level scalar key in this same file
  (alongside `GOOSE_MODE`, `OLLAMA_HOST`, etc. — the same naming convention as
  `GOOSE_TEMPERATURE`/`GOOSE_TOP_P`/`GOOSE_CONTEXT_LIMIT`/`GOOSE_CONTEXT_STRATEGY`,
  which Kitty already threads through as spawn-time env vars). Observed value on
  the probing machine: `high`. This strongly implies it's plumbable exactly like
  the other four — a `GOOSE_THINKING_EFFORT` env var at `goose serve` spawn time —
  rather than needing any live per-turn ACP call.
- **Confirmed live and correct (follow-up probe, same session)**: `session/new`
  and `session/load`'s **raw** ACP result (which Kitty's `SessionInfo` currently
  narrows down to `{sessionId, cwd, current_mode, available_modes}`, discarding
  the rest) already includes a top-level `configOptions: [...]` array — no
  extra round trip needed. Each entry: `{ id, name, category?, description?,
  currentValue, options: [{name, value}], type: "select" }`. Confirmed entries
  on the probing machine: `provider` (full provider list), `mode` (mirrors
  `available_modes`), `model` (the active provider's installed models), and
  **`thinking_effort`** — `{ id: "thinking_effort", category: "thought_level",
  name: "Thinking effort", description: "Controls reasoning effort for models
  that support extended thinking.", currentValue: "off", options: [...],
  type: "select" }`. **Crucially, `thinking_effort.options` is model-dependent**:
  for a model with no extended-thinking support (gemma4:e2b on this machine)
  it's a single-entry `[{name:"off",value:"off"}]` — i.e. "only one option
  available" *is* the signal that this model doesn't support effort control at
  all, and the UI should hide the control in that case rather than guessing a
  universal low/medium/high enum.
- **Setting it**: `session/set_config_option` `{ sessionId, configId:
  "thinking_effort", value: <one of the offered option values> }` → returns the
  same `configOptions` array with `thinking_effort.currentValue` updated. (The
  required field is `configId`, not `key`/`option`/`name` — confirmed via the
  JSON-RPC error's `data.error: "missing field \`configId\`"`, which Kitty's own
  `acp_error_message()` normally discards in favor of just `.message`; surface
  the full error object temporarily if a similar shape-guessing problem comes up
  again.) This is a live, per-session, no-restart call — unlike the
  provider/temperature/model knobs, which are still spawn-time env vars.
