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

- **Pinned/tested version:** **0.31.1** (confirmed live via `GET /api/version` and
  `ollama --version`, Stage-1 close-out).
- **Binary location:** `C:\Users\azolkover\AppData\Local\Programs\Ollama\ollama.exe` (detected).
- **Endpoints used:** `GET /api/version`, `GET /api/tags`, `POST /api/pull` (NDJSON), `DELETE /api/delete`.

## Stock Goose Desktop detection (Phase 1 `conflict.rs`)

- **Process name(s) to match:** `Goose.exe` (case-sensitive; already implemented in
  `conflict.rs` — this entry was left marked TBD after the fact, corrected in the
  Stage-1 close-out).

## Windows Copilot app (Round-2 item 2 — best-effort close after swallowing the chord)

- **Appx package:** `Microsoft.Copilot`, PackageFamilyName
  `Microsoft.Copilot_8wekyb3d8bbwe` (verified on this machine 2026-07-05).
- **Process name(s):** `mscopilot.exe` (several instances seen); also `M365Copilot`
  (the Microsoft 365 Copilot app — a *different* thing, don't target it).
- **Window class:** _not yet captured_ — no Copilot window was visible during the
  probe, so its top-level window class/title is TBD. Capture it live (e.g. with
  `EnumWindows` + `GetClassName` while a Copilot window is open) before wiring the
  defense-in-depth close in `copilot.rs`; the mitigation is best-effort and must
  not block on getting this exactly right (some OEM Copilot keys are handled by
  Windows below the `WH_KEYBOARD_LL` layer and can't be fully intercepted).

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
- **Goose installer:** _confirmed (Stage-1 close-out): there is no Windows
  `.exe`/`.msi` installer at all_ — the [releases page](https://github.com/aaif-goose/goose/releases/latest)
  (org renamed from `block/goose`; GitHub still redirects the old path) only
  publishes zip archives for Windows:
  - `goose-x86_64-pc-windows-msvc.zip` (~78 MB) — the bare CLI/`goose serve`
    binary. **This is the one Kitty needs** — it's what `locate_goose()`
    expects to find a `goose.exe` inside.
  - `goose-x86_64-pc-windows-msvc-cuda.zip` — same, CUDA-enabled build.
  - `Goose-win32-x64.zip` / `Goose-win32-x64-cuda.zip` / `Goose.zip` — the full
    **Goose Desktop** Electron app. **Do not point users at this one** — it's
    the separate GUI product `conflict.rs` already detects and warns about
    (`Goose.exe` process name); auto-installing it would risk creating the
    exact conflict Kitty is designed to flag.
  - Since none of these are silent-installable executables, the wizard no
    longer offers an "Install" button for Goose (it always would have thrown) —
    it links straight to the release page with the exact asset name to grab,
    and `install_dependency("goose")` (still reachable directly) returns a
    message with the same guidance as a defensive fallback.

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
