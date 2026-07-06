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

- **Two qualifying signals (Round-5, broadened):** the `deriveArtifact`
  heuristic in `chatStore.ts` now treats a tool call as an artifact producer if
  **either** (a) its title/toolName matches a write verb
  (`text_editor | write | create | edit | str_replace | insert | append | save |
  export | output | generate`) **or** (b) the output path it exposes ends in a
  recognized artifact extension (`csv/tsv/xlsx/xlsm/doc(x)/ppt(x)/md/markdown/
  json/jsonl/yaml/py/txt/html/xml/pdf/rtf/odt/ods/odp/ipynb/sql/toml`). The path
  still comes from `rawInput.path` (also `file_path`, or `paths[0]`). Explicit
  reads carrying a path (`rawInput.command` ∈ `view/read/list/open/cat/show/
  inspect/search/find/glob/grep`) are excluded — this also fixed a latent false
  positive where a plain `text_editor` "view" registered as a bogus artifact.
  Heuristic still errs toward false negatives, never fabricates (CLAUDE.md).

## Where files land — session cwd vs. goose's default (Round-5)

- **The per-chat working directory is honored.** Kitty passes each session's cwd
  (`Documents/Kitty/chats/<id>/`, from `resolve_cwd`) to `session/new`, and goose
  respects it: probed live (2026-07-06) `echo %CD%` from the shell tool returned
  the exact chat folder, and a relative-path `text_editor` write (`notes.txt`)
  landed there. So relative writes go to the right place per chat.
- **But the model tends to use goose's absolute default for some outputs.**
  goose's built-in default working directory is `~/Documents/Goose`; when asked
  to "export a docx," the model was writing to an absolute `~/Documents/Goose/…`
  path instead of a relative one, so exports piled up there. That's a model/goose
  behavior, not a Kitty cwd bug (the cwd is already correct).
- **Mitigation (soft nudge):** `goosed_env()` sets `GOOSE_MOIM_MESSAGE_TEXT`
  (consumed by goose's bundled `tom` / "Top Of Mind" platform extension, which
  injects it into every turn) instructing the model to save files into the
  current working directory using relative paths, not an absolute
  `~/Documents/Goose` path. Confirmed live the model receives it (quoted the text
  back verbatim) and chose a relative path afterward. It's a prompt-level nudge,
  not a hard guarantee — a model can still ignore it and write elsewhere.

## Artifact writes by provider — Round-5 diagnosis

- **Kitty imposes no restriction on what a provider can write.** There is no
  trust/tier gate on tool execution anywhere in this codebase; `is_trusted` /
  `NetworkTier` only drive UI (badges, the untrusted-attach warning). goosed
  executes tools; Kitty neither sandboxes nor filters them by file type.
- **File-writing tools are always available.** Probed live (2026-07-06): a fresh
  `session/new` already has the `developer` platform extension enabled
  ("Write and edit files, and execute shell commands") in addition to the
  `computercontroller` builtin Kitty force-adds. So `text_editor` (text writes)
  and `shell` are present in every session with no extra wiring — Kitty does
  **not** need to force-add `developer`.
- **Text formats work and appear in the Artifacts pane** (csv, md, json, py,
  txt, html, etc.): the model writes them via `developer`'s `text_editor`, whose
  tool call exposes `rawInput.path`, so `deriveArtifact` detects them.
- **Binary Office formats (xlsx/docx/pptx) are a goosed-environment concern,
  outside Kitty's control:** `text_editor` only writes text, so the model must
  generate these via `shell` running Python (`openpyxl` / `python-docx` /
  `python-pptx`). If those libraries aren't present in the environment goosed's
  shell runs in, the write fails — nothing Kitty can fix client-side. Also note
  a shell-executed write's tool call is `{command: "..."}` with no structured
  `path`, so even a *successful* shell-produced binary file won't surface in the
  Artifacts pane (the pane derives entries from tool metadata, not by scanning
  the working directory). Both are acknowledged limitations, not bugs.
- **Chat ("thought-partner") mode allows tools, scoped to the chat folder
  (Round-5, owner decision — supersedes Round-4's blanket auto-reject):** chat
  mode still forces `approve` so every tool call surfaces as a permission
  request Kitty decides in `chatStore.ts`'s approval handler. A path-based file
  op is auto-approved only if its target resolves inside the session's cwd (the
  `Documents/Kitty/chats/<id>/` folder) and auto-rejected otherwise; a tool with
  no structured path — notably `shell`, which is how docx/xlsx get produced via
  Python — is auto-approved and runs with cwd = the chat folder. **Soft
  boundary, not a sandbox:** shell isn't confined, so a command could still
  reach outside the folder; the path check hard-confines only the path-based
  ops Kitty can inspect. This is what lets a thought-partner-mode session export
  a docx (which previously hit "Tool use is off in chat mode — declined").

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
