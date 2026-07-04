# Pinned Versions & Verified API Surface

This file is the single source of truth for external-dependency versions and any
API-path / process-name assumptions the app relies on. A dependency bump is not
done until this file is updated and the affected code (`goosed/api.rs`,
`conflict.rs`, `starter_models.ts`, etc.) is re-verified against it.

## Goose

- **Pinned version:** **1.41.0** (CLI reports `1.41.0`; bundled Desktop `version` file `41.0.0`). Dev binary already on disk — pin to this.
- **goose binary location (dev):** `C:\Users\azolkover\AppData\Local\Programs\dist-windows\resources\bin\goose.exe` (bundled inside the installed Goose Desktop app). Also present there: `uv.exe`, `uvx.exe`. Desktop shell: `...\dist-windows\Goose.exe`.
- **Install method used for dev:** existing Goose Desktop install (no separate install needed). Block's Goose is **not on winget** (`Pressly.Goose` is an unrelated DB-migration tool).
- **⚠️ API surface — ACP, not legacy REST:** this version has **completed** the migration CLAUDE.md warned about. There is **no `goosed agent` command and no `/reply` REST API**. Server subcommands:
  - `goose serve` — **ACP (Agent Client Protocol) over HTTP + WebSocket**. Default host `127.0.0.1`, **default port `3284`**. Flags: `--host`, `--port`, `--tls[/-cert-path/-key-path]`, `--platform cli|desktop`, `--with-builtin <names>`, `--allowed-origin <origin>`, `--dangerously-unauthenticated`.
  - `goose acp` — ACP over **stdio** (`--with-builtin` only).
  - Secret-key auth **still applies**: `GOOSE_SERVER__SECRET_KEY` (the `--dangerously-unauthenticated` flag opts out). CLAUDE.md's secret-key model holds; the *transport/protocol* changes.
- **Consequence for the plan:** integration targets **ACP JSON-RPC** (`session/new`, `session/prompt`, streamed `session/update` notifications, permission/approval requests, session loading), **not** REST routes `/agent`,`/reply`,`/sessions`,`/config`. No `openapi.json` to vendor — vendor/pin the **ACP schema + method list** instead. Architecture (Rust owns I/O, Tauri events to frontend) is unchanged; the change is confined to `goosed/api.rs` + `goosed/stream.rs` — the isolation boundary CLAUDE.md designed for.
- **Streaming/reasoning surface:** ACP `session/update` **does** surface a distinct reasoning variant — `agent_thought_chunk` (separate from `agent_message_chunk`). Phase 10 renders it directly; no `<think>` splitting needed.
- **Full ACP method/transport reference:** [acp-protocol.md](acp-protocol.md) (confirmed live 2026-07-04). Transport = WebSocket `ws://127.0.0.1:<port>/acp?token=<secret>`; readiness = `GET /status` + `X-Secret-Key`.

## Ollama

- **Pinned/tested version:** _TBD._
- **Binary location:** `C:\Users\azolkover\AppData\Local\Programs\Ollama\ollama.exe` (detected).
- **Endpoints used:** `GET /api/version`, `GET /api/tags`, `POST /api/pull` (NDJSON), `DELETE /api/delete`.

## Stock Goose Desktop detection (Phase 1 `conflict.rs`)

- **Process name(s) to match:** _TBD (record exact names after checking the pinned release)._

## File-writing tool names (Phase 4 artifacts heuristic)

- **Tool-name patterns treated as artifact producers:** developer `text_editor`
  tool with a write-like command; match tool title/name against
  `text_editor | write | create | edit | str_replace`, and take the file path
  from `rawInput.path` (also `file_path`, or `paths[]`). Heuristic — false
  negatives are acceptable, never fabricate entries (CLAUDE.md).

## Installer URLs & hashes (Phase 7)

- **Ollama Windows installer:** `https://ollama.com/download/OllamaSetup.exe`
  (Inno Setup; hands off to its own UI/UAC — verify a silent flag before enabling
  unattended install). Wired in `src-tauri/src/wizard.rs`.
- **Goose installer:** _TBD — Block's Windows installer asset URL from
  block/goose GitHub releases. Not wired yet; the wizard tells the user to install
  Goose Desktop and re-detect. (The dev machine already has it under
  `%LOCALAPPDATA%\Programs\dist-windows`.)_

## Starter models (Phase 7 `src/lib/starter_models.ts`)

- **Curated ≤4B list:** `llama3.2:1b` (~1.3GB), `llama3.2:3b` (~2GB),
  `qwen2.5:3b` (~1.9GB), `gemma2:2b` (~1.6GB). **Re-verify these tags exist on
  ollama.com before release** — the dev environment is future-dated (Ollama
  0.31.1, models `gemma4`/`qwen3.5`), so newer small tags may be preferable.

## Reasoning-capable models (Phase 10 `src/lib/reasoning_models.ts`)

- **Name patterns → supports-reasoning:** `think` (lfm2.5-thinking, *-thinking),
  `reason`, `deepseek-r1`, `qwq`, `magistral`, OpenAI `o1/o3/o4`, bare `r1`.
- This table only drives the *predictive* thinking indicator. The reasoning panel
  is **content-driven** (shown whenever a model actually emits `agent_thought_chunk`),
  so models that reason but don't match a pattern (e.g. `gemma4:e2b`, which emits
  thoughts here) still get a panel. ACP surfaces reasoning as a distinct
  `agent_thought_chunk` — no `<think>` tag splitting needed (see acp-protocol.md).
