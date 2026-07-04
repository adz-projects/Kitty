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
   - Working-dir change on an existing session is the *unstable* extension method
     `_goose/unstable/session/working-dir/update` — we avoid it; "Set as working
     directory" starts a new `session/new` rooted at the folder instead.
5. `session/prompt` `{ sessionId, prompt: [ContentBlock] }`
   - streams `session/update` notifications, then result: `{ stopReason, usage: { totalTokens, inputTokens, outputTokens } }`
   - `stopReason`: `end_turn | max_tokens | max_turn_requests | refusal | cancelled`.
6. `session/cancel` `{ sessionId }` — notification (Phase 8).

`ContentBlock`: `{ type: "text", text }` (also image/audio per promptCapabilities).

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
- `fs/read_text_file`, `fs/write_text_file` — only if we advertise the capability
  (we set both `false` for now), so respond method-not-found otherwise.
