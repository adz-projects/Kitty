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
4. `session/load` `{ sessionId, cwd, mcpServers }` — resume (Phase 4).
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

- `session/request_permission` `{ sessionId, toolCall, options: [{ optionId, name, kind }] }`
  → respond `{ outcome: { outcome: "selected", optionId } }` or `{ outcome: { outcome: "cancelled" } }`.
  Option ids observed: `allow_once`, `allow_always`, `reject_once`. Only sent in
  `approve` / `smart_approve` modes — **Phase 3** wires the real UI; in `auto`
  mode (default) it is never sent.
- `fs/read_text_file`, `fs/write_text_file` — only if we advertise the capability
  (we set both `false` for now), so respond method-not-found otherwise.
