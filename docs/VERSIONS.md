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

## Windows Copilot app (REMOVED — UX-simplification pass)

The hardware Copilot-key hook (`copilot.rs`, the `use_copilot_key` config field,
and the `WH_KEYBOARD_LL` chord-swallow described below) was removed by owner
decision during the UX-simplification pass — one low-level global keyboard
hook was judged not worth the complexity/risk versus a configurable hotkey,
which already does the same summon job. The section below is kept for
historical context only; none of it reflects current code.

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
  shell runs in, the write fails — nothing Kitty can fix client-side. A
  shell-executed write's tool call is `{command: "..."}` with no structured
  `path`, so `deriveArtifact`'s tool-metadata derivation still misses it — but
  (Round-7 item 5) the Artifacts pane also disk-scans the session's working
  directory (`refreshArtifactsFromDisk`/`list_directory`), so a successful
  shell-produced binary now does surface there, just via the disk scan rather
  than the tool-call path.
- **Chat ("thought-partner") mode allows tools, scoped to the chat folder
  (Round-5, owner decision — supersedes Round-4's blanket auto-reject; made
  tri-state in Round-7 item 3):** chat mode still forces `approve` so every
  tool call surfaces as a permission request `decideChatApproval`
  (`stores/chat/approvalUtils.ts`) decides. It returns one of three
  decisions — `allow`/`reject`/`prompt` — not just a boolean: a path-based file
  op resolving inside the session's cwd (the `Documents/Kitty/chats/<id>/`
  folder) is `allow`; one resolving outside it, or a shell command matching
  `isSecuritySensitiveCommand` (ssh/scp/sudo/`rm -rf`/chmod/curl -o/etc.), is
  `prompt` — queued to `pendingApprovals` for the user to decide, same as
  agentic mode, rather than auto-rejected outright; anything else (notably most
  `shell` calls, which is how docx/xlsx get produced via Python) is `allow` and
  runs with cwd = the chat folder. **Soft boundary, not a sandbox:** shell isn't
  confined, so a command could still reach outside the folder; the path check
  and sensitive-command check only hard-confine what Kitty can actually
  inspect. This is what lets a thought-partner-mode session export a docx
  (which previously hit "Tool use is off in chat mode — declined") while still
  surfacing a human decision point for genuinely risky commands instead of
  silently running them.

## Installer URLs & hashes (Phase 7; Goose auto-install added in the wizard redesign)

- **Ollama Windows installer:** `https://ollama.com/download/OllamaSetup.exe`
  (Inno Setup; hands off to its own UI/UAC — verify a silent flag before enabling
  unattended install). Wired in `src-tauri/src/wizard.rs`.
- **Goose:** _confirmed (Stage-1 close-out): there is no Windows `.exe`/`.msi`
  installer at all_ — the [releases page](https://github.com/aaif-goose/goose/releases/latest)
  (org renamed from `block/goose`; GitHub still redirects the old path) only
  publishes zip archives for Windows:
  - `goose-x86_64-pc-windows-msvc.zip` (~78 MB) — the bare CLI/`goose serve`
    binary. **This is the one Kitty needs and installs automatically.**
  - `goose-x86_64-pc-windows-msvc-cuda.zip` — same, CUDA-enabled build. Not
    used by the auto-install (the plain build is the safe default).
  - `Goose-win32-x64.zip` / `Goose-win32-x64-cuda.zip` / `Goose.zip` — the full
    **Goose Desktop** Electron app. **Never auto-install this one** — it's
    the separate GUI product `conflict.rs` already detects and warns about
    (`Goose.exe` process name); installing it would create the exact conflict
    Kitty is designed to flag.
  - **Wizard redesign (current behavior):** since there's no installer
    executable to silently run, `wizard::install("goose")`
    (`src-tauri/src/wizard.rs`) instead resolves the CLI zip's real download
    URL from the GitHub Releases API by exact asset name
    (`GOOSE_CLI_ASSET_NAME`), downloads it, extracts it via the `zip` crate
    into `%LOCALAPPDATA%\Kitty\goose\`, and persists the extracted
    `goose.exe` path as `Config.goose_binary_override` — which
    `lifecycle::goosed::locate_goose` now checks first, before the env var /
    Goose Desktop bundle path / bare PATH fallbacks. The wizard's Detect step
    calls this from a real "Install" button; a "I already have it" fallback
    (native `.exe` file picker) sets the same override manually for a user
    who's already got Goose somewhere non-standard.

## Wizard redesign: local-vs-API-key fork, `ollama_enabled` (2026-07-11)

- **The wizard's first screen now forks**: "Run models on this computer"
  (existing Detect/Configure/First-model flow, Ollama+Goose auto-install) vs.
  "Use my own API key" (new step reusing `Providers.tsx`'s save/activate
  infra — Anthropic/OpenAI/OpenRouter/Custom, base URLs from the shared
  `src/lib/provider_defaults.ts`). First-party API-key providers created this
  way are marked `is_trusted: true` immediately (owner decision — no scary
  ⚠ badge for a key the user just pasted on purpose); `custom_openai` stays
  untrusted by default, same as adding one from Settings.
- **`Config.ollama_enabled`** (default `true`, so pre-existing installs are
  unaffected): set explicitly by the wizard's fork. `false` hides Settings →
  "Ollama Models" and the "Ollama" option in Add Provider's type picker, and
  `start_stack`/`compute_status` (`config::providers::requires_local_ollama`)
  stop trying to reach Ollama at all. Settings → Advanced has an "Enable &
  install Ollama" action that flips it back on and runs the same install path
  the wizard uses.
- **`validate_setup` command** (`src-tauri/src/commands/setup.rs`) is the
  single source of truth for "is this setup actually ready to chat" — checks
  the active provider has a model (and, for remote types, a stored key) plus
  a fresh `compute_status`. Powers the wizard's Done-step summary and its
  soft Finish-anyway gate (never a hard block), and Setup & Repair's re-check.
- **Adaptive Pathway auto-install (near-term bridge):** the wizard now
  attempts a best-effort `pip install adaptive-pathway[sidecar]` if the
  console scripts aren't already resolvable (`wizard::install_adaptive_pathway`).
  Failure is non-fatal — the extension just stays `Down`, same graceful
  degradation as always, with a manual retry in Settings → Advanced. The
  real, owner-specified target is bundling a standalone sidecar executable as
  a Tauri `externalBin` sidecar (no Python dependency at all) — not yet
  built; this pip-based path is an explicit bridge until that lands.

## Starter models (Phase 7 `src/lib/starter_models.ts`)

- **Curated VRAM-tiered list** (re-verified on ollama.com, replacing the
  earlier ≤4B `llama3.2`/`qwen2.5`/`gemma2` set now that `gemma4`/`qwen3.5`
  are current): `gemma4:e2b` (7.2GB — 4-8GB VRAM), `qwen3.5:4b` (3.4GB — 8GB
  VRAM), `gemma4:e4b` (9.6GB — 8GB VRAM), `qwen3.5:9b` (6.6GB — 16GB VRAM).
  Re-verify tags/sizes again before release if enough time has passed for the
  lineup to have moved again.

## Reasoning-capable models (Phase 10 `src/lib/reasoning_models.ts`)

- **Name patterns → supports-reasoning:** `think` (lfm2.5-thinking, *-thinking),
  `reason`, `deepseek-r1`, `qwq`, `magistral`, OpenAI `o1/o3/o4`, bare `r1`.
- This table only drives the *predictive* thinking indicator. The reasoning panel
  is **content-driven** (shown whenever a model actually emits `agent_thought_chunk`),
  so models that reason but don't match a pattern (e.g. `gemma4:e2b`, which emits
  thoughts here) still get a panel. ACP surfaces reasoning as a distinct
  `agent_thought_chunk` — no `<think>` tag splitting needed (see acp-protocol.md).

## Internal plugins & build tooling (`plugins/`)

- **Python:** 3.11 (matches the `.pyc` cache tags already present in the
  vendored Adaptive Pathway source — re-verify against whatever interpreter
  actually runs `plugins/build.py` at release time).
- **PyInstaller:** installed on demand by `plugins/build.py`
  (`pip install pyinstaller`, no version pinned yet — pin here once a
  specific version is confirmed to freeze both plugins cleanly).
- **Adaptive Pathway** (`plugins/adaptive-pathway/`): freezes the `sidecar`
  extra only (fastapi/uvicorn/numpy/aiosqlite/sqlalchemy/pyyaml/mmh3) — the
  `full` extras (onnxruntime/hdbscan/bertopic) are excluded from the frozen
  binary since Kitty doesn't surface the clustering/topic features that need
  them; revisit if that changes.
- **replacement-mcp** (`plugins/replacement-mcp/`): freezes `lean_mcp.py`'s
  `main()` — deps: fastmcp, httpx, trafilatura, ddgs, openpyxl,
  python-docx, pypdf, pyyaml (see `plugins/replacement-mcp/pyproject.toml`).
  The search dep is **`ddgs` (>=9.0), not `duckduckgo-search`** — the latter
  was renamed, and its last releases (8.x) return zero results for every
  query against DuckDuckGo's current backend, which surfaced in chat as
  `lean_fallback_web_search` reporting "No results found" no matter what was
  asked. Verified working on ddgs 9.11.3; the API (`DDGS().text(query,
  max_results=…)` → `title`/`href`/`body`) is unchanged from 8.x.
- Both freeze to `src-tauri/binaries/<name>-x86_64-pc-windows-msvc.exe` and
  are declared in `src-tauri/tauri.conf.json`'s `bundle.externalBin`. The
  committed files at that path are **empty placeholders** (satisfy Tauri's
  build-time existence check for local `cargo build`) until
  `python plugins/build.py` overwrites them with real frozen executables —
  see `src-tauri/binaries/README.md`.
