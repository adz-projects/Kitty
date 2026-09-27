# CLAUDE.md — Kitty

## What this project is

A Tauri v2 app on **two targets — Windows and Android** — backed by **BigTiny
V2** (a chat-first REST/SSE agent engine, a pure-Rust crate at
`BigTinyV2/daemon/`). On Windows it is a hotkey-summoned floating overlay chat
client plus a full window; on Android it is a single routed window with a menu
drawer. It is a **client only** — agent logic, tool execution, MCP handling,
scheduling and model routing stay inside the engine. This app owns: window
management (overlay / hub / tray), attaching to the engine, configuration UI,
drag-and-drop and shared file context, session history UI, an artifacts pane,
notifications, the tool-approval UI, theming, and a first-run wizard.

`plugins/bigtiny_rust/` is the frozen V1 engine: nothing builds or links it,
and it is kept only as a rollback path. A V1 install's data is imported with
one click (`commands/v1_import.rs`). Features exist only in V2.

**The engine is shared.** Other apps (a research pipeline, an AI notebook)
attach to the same V2 engine on desktop, each with its own sessions, providers,
MCP servers and schedules. Changes Kitty needs in V2 are therefore additive and
app-scoped, and V2 stays on the 2.x line.

**Local inference is LiteRT linked into the engine** (`BigTinyV2/daemon/src/litert/`),
not llama.cpp and not Ollama. There is **no local chat** — chat always goes to
a remote provider. LiteRT does two local jobs: **semantic embeddings** for the
memory engines (EmbeddingGemma `.tflite`, both platforms; the Gemma
`tokenizer.json` is downloaded with it) and, **on Windows only**, the optional
local **summarizer** for compaction (LiteRT-LM running
`gemma-4-E2B-it.litertlm`). Android summarizes with the chat's own provider.
Kitty manages no inference process: models come from Hugging Face through
`src-tauri/src/models/` (EmbeddingGemma is licence-gated, so the download takes
a transient HF token). `provider_type: "ollama"` survives only as a *remote*
endpoint dialect for a server the user runs.

**The two targets differ in hosting, not in code.** Desktop attaches to a
shared `bigtiny2-daemon.exe` (spawning one only if none is running) and bundles
the MCP servers as `externalBin` sidecars. Android links the same engine and
hosts it in-process (`lifecycle/bigtiny_embedded.rs`) with `transport:
"in_process"` MCP servers, because Android 10+ refuses to `exec()` a binary in
app-writable storage. Both sit behind the same HTTP boundary, so nothing above
`lifecycle/` knows the difference. `docs/ANDROID.md` is the Android plan of
record and `docs/RELEASE.md` has both packaging lanes.

Goose is not part of this app — BigTiny is the only backend, and the UI never
names it (or Goose) as a dependency. **`docs/ARCHITECTURE.md` is the accurate
module map**; this file plus that one are the spec.

## Tech stack (do not deviate without asking)

- **Shell**: Tauri v2, targeting Windows (`nsis`) and Android (`aarch64-linux-android`, AAB). Rust backend ("core"), web frontend.
- **Frontend**: React 18 + TypeScript + Vite. Plain CSS with CSS custom properties for theming — **no Tailwind, no CSS-in-JS** (theming requirement is user-droppable plain CSS).
- **State**: Zustand for UI state. No Redux.
- **Rust crates**: `tauri`, `tauri-plugin-global-shortcut`, `tauri-plugin-notification` (desktop), `tauri-plugin-shell`/`-opener`, `tauri-plugin-dialog`, `tauri-plugin-single-instance`, `reqwest` (with `stream`), `tokio`, `serde`/`serde_json`, `keyring` (Windows Credential Manager; not on Android), `windows` (Win32), `thiserror`, and the engine's own `bigtiny2-client` / `bigtiny2-protocol`.
- **HTTP to the engine**: all network calls go through the Rust side (Tauri commands + events). The webview never fetches localhost directly — this keeps the app key out of JS and avoids CORS issues.
- **Exception, by design**: the MCP servers under `plugins/` (see "Internal
  plugins" below) and the engine itself ship as part of the app, built with
  plain `cargo build --release` and bundled through Tauri's `externalBin` on
  desktop. That is a sanctioned part of the stack, not a deviation. It does
  **not** mean "add a dependency-heavy plugin freely" — a new plugin needs the
  same build-and-bundle treatment (`docs/PLUGINS.md`), and the app itself
  (Rust core + React frontend) stays exactly as described above.

## The engine API this app consumes

- **Desktop**: Kitty reads the engine's handshake file, checks the process is a
  V2 engine of a compatible API version (2), and spawns one under a lock only
  when none is running. It registers once as the app `kitty` and keeps the
  issued **app key** in the Credential Manager; every call sends it as
  `X-API-Key`. A lost key is reclaimed with the handshake's registration token
  (`POST /api/apps/reclaim`), keeping Kitty's data. `GET /api/health` is open
  without a key, for readiness polling. Kitty never kills the engine: it asks
  it to restart (`POST /api/admin/restart`) when a start-up setting changed,
  and the engine refuses while other apps are attached or busy.
- **Android**: the same engine runs in-process on a loopback port; Kitty
  registers the same way, and supplies the at-rest key from the SecretStore.
- **Routes**: plain REST plus SSE streams — the chat turn
  (`POST /api/chat/{id}/send`, `/stream` to follow a turn running elsewhere)
  and the app-scoped event stream (`GET /api/apps/me/events`: approvals,
  schedule runs, titles). Both send keepalives every 15 s. Route families:
  sessions (`/api/chat/...`), search, providers, MCP servers, specialists,
  schedules, HITL rules, memory (`/api/pathway/*`, `/api/memorabilia/*`),
  per-app plugins and data (`/api/apps/me/...`: plugins, pending approvals,
  V1 import, purge). All paths live in `src-tauri/src/bigtiny/` and the
  `lifecycle/` modules that attach, so an API change touches one place.
- `BigTinyV2/API.md` is the source of truth for route shapes.

## Internal plugins (`plugins/`)

Subsystems ship as **internal plugins**: independent, tested Rust packages in
this repo under `plugins/`, built to standalone executables for desktop
(`externalBin`) and linked in-process on Android. End users need no runtime of
any kind. `python plugins/build.py` builds them and the engine and writes
`src-tauri/binaries/manifest.json`; `--verify-manifest` checks the committed
binaries still match their source (CI runs it). Full detail in
`docs/PLUGINS.md`; the current plugins:

- **`adaptive-pathway_rust`** — the behavioural-memory engine, linked into the
  engine (not a binary of its own). Extracts durable beliefs about the user
  from conversation, decays and consolidates them, and injects a DPP-selected
  handful per turn — framed as working assumptions to check the request
  against, never a profile to conform to (see `docs/VERSIONS.md`'s
  recall-framing contract). Its `record`/`forget` tools reach the model through
  the engine's in-process MCP registry. Settings: belief browser, Graph
  Health, Erase all; per-chat incognito in the chat's ⋯ menu.
- **`memorabilia_rust`** — the factual-memory engine, also linked in. Learns
  from the documents a turn brings in (pasted text, attached files, scraped
  pages), not from the dialogue. Desktop only.

  Both memory engines run only when the embedding model is on disk **and** the
  user has them on; Kitty switches them per app, at once
  (`lifecycle/memory.rs`). Without the model, chat is unaffected and memory is
  simply off.
- **`kitty-tools`** — MCP server registered in the engine's own
  `/api/mcp/servers`, not spawned by Kitty (Kitty keeps the registration's
  command path and `enabled` flag in sync with Settings:
  `bigtiny::mcp::ensure_builtin_servers`). **26 tools on desktop, 24 on
  Android** (no `lean_shell` / `lean_shell_ro` there): workspace, 5 file, 3
  Word, 2 Excel, 2 PDF, image reading, 4 scratchpad, 4 cache, 2 document-handle,
  shell — on by default, since they are what makes small models usable as
  agents — plus **4** visualization tools (table, SVG diagram, chart, Mermaid)
  behind their own Settings toggle.

  Every paged reader (`lean_file_read`, `lean_word_read_text`,
  `lean_pdf_read_text`) writes through an **extract-once cache**
  (`plugins/kitty-tools/src/doc_store.rs`) keyed by the file's
  `(path, len, mtime)` and returns a `document_id`; `lean_doc_read_chunk` and
  `lean_doc_search` read the rest of that extraction without re-parsing the
  source. The id derives from the fingerprint, so an unchanged file keeps its
  handle and an edited one can never be served a stale extraction.
- **`kitty-web`** — same registration pattern. `lean_web_scrape` (which also
  **downloads** a URL that returns a document — pdf, docx, xlsx, csv, txt, md,
  json, xml, yaml — to the shared cache and hands back `cached_path`), and
  `lean_web_search` / `lean_web_search_read_chunk`: Brave per query when a
  Brave key is configured (off by default; needs the web server on), otherwise
  DuckDuckGo **and Bing** together, with language and country honoured on
  both. Bing is co-equal rather than a fallback because DuckDuckGo serves a
  **bot challenge** under concurrent load with an HTTP 202, which reads as an
  empty page unless detected. `plugins/kitty-web/src/ratelimit.rs` paces the
  scraped engines process-wide.
- **`kitty-wasm`** — same pattern. 4 tools running Python or any WASI module
  in a wasmtime sandbox with time/memory ceilings, no network, and no
  filesystem beyond explicit mounts. Its CPython guest is bundled on Windows;
  on Android it downloads once on first use.

On Android the three MCP servers are configured through an explicit
`InProcessConfig` (grants, plugin home, keys), never the process environment.
See `docs/PLUGINS.md` for the two integration shapes (Kitty-managed vs.
engine-managed) and why mixing them is a bug.

## Other subsystems

- **Approvals** (`approvals.rs`, `lifecycle/app_events.rs`,
  `stores/approvalStore.ts`, `ApprovalModal`) — the engine pauses on tool
  calls; Kitty answers the safe ones itself (file operations inside the chat's
  granted folders, shell commands that are not security-sensitive) and puts the
  rest in front of the user: inline in the chat if it is on screen, a blocking
  dialog in whichever window is, the overlay summoned if none is, plus a
  notification. "Always allow" is scoped (the command's first two words for a
  shell call, else the tool) and listed with Revoke in Settings → Tool
  permissions. A scheduled run's approval is denied after 10 minutes unanswered.
- **Specialists** (`bigtiny/specialists.rs`, `commands/specialists.rs`,
  `components/settings/Specialists.tsx`) — delegate agents the *model* calls
  mid-turn via `call_specialist`, each in its own session with its own model,
  tool allow-list, answer schema, reasoning cap, fan-out and concurrency limit.
  Definitions live in the engine (`/api/specialists`). Kitty owns the authoring
  UI, the per-provider "use for specialists" setting, the overall time and
  concurrency limits, and the subagent model denylist (relayed at start-up,
  seeded once from the OpenRouter catalog's premium tier after the first
  OpenRouter provider is synced). Delegates run in the background: the model
  collects reports with `await_specialists`, and a turn cannot end until every
  ticket is collected (`docs/VERSIONS.md`, 0.10.7). Desktop only.
- **Scheduled tasks** (`commands/scheduled_tasks.rs` over `/api/schedules`,
  `components/settings/ScheduledTasks.tsx`) — instructions the engine runs
  later, once or on an interval, on a chosen provider card with its system
  prompt. On desktop they run whether or not Kitty is open; on Android they run
  while Kitty's process is alive, and one that fell due meanwhile runs when
  Kitty next opens (headless runs: `docs/BACKLOG.md`). Settings shows the last
  run with a link to its chat, and "Run now". Failures notify.
- **Providers** — each card is one provider + one model (Duplicate to add
  another model); every card is synced into the engine, and the default is
  just a flag. A chat stays on its card; moving it is a branch onto another
  card, through the handoff gate when the target is less trusted.
- **Folder bookmarks** (`commands/folders.rs`) — app-side session organization
  over the engine's flat session list.
- **Log capture** (`log_capture.rs`) — an in-memory ring buffer of
  `warn!`/`error!` events, shown in Settings → Advanced with Copy and Save.

## Repository layout

`docs/ARCHITECTURE.md` has the module map. At the top level:

```
CLAUDE.md, AGENTS.md, README.md
docs/            ARCHITECTURE, ANDROID, PLUGINS, RELEASE, VERSIONS, BACKLOG, ...
src/             React frontend (windows/, components/, stores/, lib/, themes/)
src-tauri/       Rust core (src/), Tauri config, bundled binaries, Android project (gen/android)
BigTinyV2/       the engine: daemon/, client/, protocol/
plugins/         kitty-tools, kitty-web, kitty-wasm, the two memory engines,
                 build.py; bigtiny_rust/ is frozen V1
```

## Core architectural rules

1. **One Rust process, multiple windows.** Windows are Tauri WebviewWindows. `hub` is the routed window — chat, saved chats, settings and the wizard are views inside it (`routeStore`), not separate labels — and multiple hubs can be open at once (`chat-N`). `overlay` and `screenshot-select` are desktop-only; the overlay is created hidden at startup and toggled (show/hide), never destroyed, because summon latency is the product. Android runs exactly one hub and nothing else.
2. **All I/O in Rust.** Frontend calls `invoke()` via `src/lib/ipc.ts` only. Streaming and push data is forwarded as Tauri events, e.g. `chat://message-delta`, `chat://tool-call`, `chat://notice`, `chat://complete`, `chat://error`, `approval://needed`, `approval://resolved`, `session://renamed`, `config://changed`, `memory://status`, `models://progress`, `stack://status`, `engine://restart-state`. Event payloads include `session_id` (or `download_id` / `action_id`) so multiple listeners can filter.
3. **Session state lives in the engine.** The frontend keeps only render state. Resume = fetch the session from the engine and re-render. Never persist chat history app-side.
4. **Secrets never touch JS or plaintext disk.** API keys and Kitty's app key go in the Windows Credential Manager via `keyring` (service = `kitty`; a provider's account is its profile id). App config JSON stores profile metadata only (name, provider type, base URL, model, trust) — never keys. The engine holds the provider keys it needs sealed under its own key, which on Windows it keeps DPAPI-protected (`encryption.key.dpapi`). Android uses a different store for the same contract: `keyring` has no Android backend at all (it silently degrades to an in-memory mock, which is what made D24 a silent-data-loss bug), so secrets there — including the in-process engine's key — are AES-256-GCM sealed under a non-exportable AndroidKeyStore key (`gen/android/.../SecretStore.kt` behind `src/android/secrets.rs`). `keyring` is excluded from the Android dependency graph so the mock cannot come back.
5. **Overlay and hub share the chat implementation.** `components/chat/*` renders in both; the window entry decides chrome. Both can show the same session and stay in sync, because both consume the same events keyed by session id. The same components render on Android — platform differences are `isAndroid()` gates and the mobile CSS breakpoint, never a forked component tree.
6. **There is no chat/agent mode.** What a session can do is a property of its *provider*: a provider that can't call tools (auto-detected from the catalog, or set on the card) simply never gets any. A long paste collapses into a chip, and a dropped file is inlined as content whenever the provider has no tools to open a path with.
7. **Errors are states, not toasts.** `stackStore` holds a machine-readable status (`starting | ok | backend_down`, plus a `detail`). The chat renders a status panel whenever it isn't `ok`. Provider problems belong to the chat that hit them: its error card names the chat's own provider and links to it in Settings, and an unreachable provider shows a banner in the chats on that card only, clearing itself when it answers.

## Coding conventions

- Rust: `rustfmt` defaults, `clippy` clean (desktop and `cargo ndk -t arm64-v8a clippy`), `thiserror` for error enums per module; every `#[tauri::command]` returns `Result<T, String>` with user-safe messages (log details with `tracing`, don't surface internals).
- TS: strict mode, ESLint + Prettier. No `any`. Components are function components; hooks in `src/hooks/` if shared. In-app dialogs (`Dialog`, `confirmDialog`), never `window.confirm`.
- CSS: all colors/spacing/typography via custom properties defined in theme files; `base.css` may not contain color values.
- Commit per task, conventional commits (`feat:`, `fix:`, `chore:`).
- Tests: Rust unit tests for config/providers/engine stream+mcp+session translation; frontend vitest for stores, the ipc wrapper and pure helpers.

---

# Phased implementation plan (HISTORICAL — original build order, not current state)

**Everything below this line describes the original goosed/ACP-based design and
is retained only as a historical record of the project's build order.** Do not
use it as a reference for how anything works now — see `docs/ARCHITECTURE.md`
and the sections above. In particular:

- Every goosed-specific mechanism (`goosed/api.rs`, `goose serve`, ACP, Goose
  Desktop conflict detection, the wizard's Goose install step, goose's
  `config.yaml`) is **gone**; the backend is BigTiny V2.
- **Ollama management is gone** (Phases 1, 5, 7): Kitty installs, starts and
  pulls nothing for Ollama; local models are LiteRT helper models downloaded
  from Hugging Face.
- **Approval modes are gone** (Phase 3's mode badge): approvals follow one
  fixed policy, with scoped "Always allow" rules revocable in Settings.
- **The Copilot-key hook and the background image were dropped** (Phase 6):
  hotkeys are ordinary global shortcuts, and themes are CSS files.
- **The chat-only mode is gone** (Phase 9, see rule 6), and so are strict
  remote mode and the "strip reasoning" option.
- The **context handoff gate** now guards *branching* a chat onto a
  less-trusted provider, and can remember its answer (Phase 5 said "no
  remember-choice", which was reversed).
- **ChatML export** is built by Rust from the engine's stored history (Phase
  11's format, with reasoning persisted by the engine).

The phase-by-phase scope is still a fair map of what the product does.

Do phases in order. Each phase must compile, run, and pass its acceptance checks before starting the next.

## Phase 0 — Skeleton & plumbing

Scope:
- Scaffold Tauri v2 + React + Vite project matching the repo layout above. Windows-only config (`bundle.targets = ["nsis"]`).
- Single-instance plugin: second launch focuses/toggles the overlay of the first instance.
- Create all four windows (overlay hidden, frameless, transparent, always-on-top, skip-taskbar; main/settings/wizard normal, hidden until used).
- Tray icon with menu: Toggle Overlay, New Session (stub), Open Settings, Quit.
- Global shortcut default `Alt+Space` toggles the overlay (Copilot key comes in Phase 6). Escape hides the overlay when focused.
- App config module: load/save JSON at `%APPDATA%/goose-overlay/config.json` with serde defaults; expose `get_config`/`set_config` commands.
- `ipc.ts` with typed invoke wrappers; `stackStore` scaffolding.

Acceptance: app launches to tray, hotkey toggles an empty overlay in <150ms after first show, settings window opens from tray, config file round-trips, `cargo clippy` and `pnpm lint` clean.

## Phase 1 — Process lifecycle & health

Scope (`src-tauri/src/lifecycle/`):
- On startup: ensure Ollama is running (probe `GET /api/version`; if down and an `ollama` binary exists, spawn `ollama serve` as child; track whether we own the process). Then spawn `goosed agent` with generated secret key + free port; store both in `AppState`.
- Health loop (tokio task, every 5s): probe ollama version + a cheap goosed route; update stack status and emit `stack://status` on change.
- On exit (tray Quit and window-close-to-tray semantics): kill child processes **we** spawned; leave pre-existing ones alone.
- Conflict detection (`conflict.rs`): via `sysinfo`, detect a running stock Goose Desktop process (process name match; record exact names in `docs/VERSIONS.md` after checking the pinned release). If found, emit `stack://status` with `conflict_goose_desktop`; frontend shows a non-blocking warning banner.
- Degraded-state UI: overlay chat area renders status panel + "Fix this" button when status != ok (button opens settings; deep-link targets wired fully in Phase 5).

Acceptance: killing goosed manually flips the UI to degraded within 5s and it recovers when the app restarts it (add a `restart_goosed` command + button); quitting the app leaves no orphan children we spawned; starting stock Goose Desktop shows the banner.

## Phase 2 — Chat MVP against goosed

Scope:
- `goosed/api.rs`: session create, agent start, and the streaming reply call, typed against the vendored spec.
- `goosed/stream.rs`: consume the SSE/streaming reply with `reqwest`, parse events, forward as Tauri events (rule 2). Handle: message text deltas, tool-call started/completed (name, params, result), completion (with token usage if present), errors, and the approval-required event.
- Chat UI in overlay: composer (Enter sends, Shift+Enter newline), streaming message list with markdown rendering (`react-markdown` + `rehype-highlight`), tool calls rendered as collapsible `ToolCallCard`s (name + params, expandable result).
- New Session from tray and from a button in the overlay; session id held in `chatStore`.
- Context pill (static for now): shows the session working directory.
- Full window (`main`): same chat components, plus an "expand" button in the overlay that opens `main` bound to the same session and hides the overlay.

Acceptance: with Goose configured for Ollama + a pulled model, user can hold a multi-turn conversation with streamed tokens, ask it to run a shell command and see the tool card, expand to full window mid-conversation and continue the same session.

## Phase 3 — Tool approvals & notifications

Scope:
- Approval flow: on `chat://tool-approval-needed`, render `ApprovalPrompt` inline (tool name, full params — for shell, the exact command in a code block; Approve / Deny buttons) and call the corresponding goosed confirm/deny endpoint (find it in the spec; the desktop app's permission confirmation route is the reference).
- Mode indicator: current approval mode (auto / smart-approve / manual, read from Goose config) shown as a small badge near the composer; clicking it opens a popover to switch modes (writes through goosed config routes).
- Notifications (`notifications.rs` + `tauri-plugin-notification`), fired only when the relevant window is hidden: task complete, approval needed, task failed, stack degraded. Clicking one shows the overlay focused on the pending item (approval prompts scroll into view).
- Tray icon state: swap icon (or overlay badge) when an approval is pending or a task is running.
- Settings model for notification prefs (per-event toggles) — stored in app config; the settings UI panel for it lands in Phase 5, but wire the checks now.

Acceptance: in manual mode with the overlay dismissed, a tool call raises a Windows notification; clicking it summons the overlay at the approval prompt; nothing executes until approved; deny cleanly cancels and the model receives the denial.

## Phase 4 — Filesystem context, sessions, artifacts

Scope:
- Drag-and-drop: Tauri window file-drop events on overlay + main. Dropped paths become removable chips in the composer (file vs. folder icon, basename shown, full path in tooltip). On send, prepend a structured context block to the message text: `Files provided by the user:\n- <path>\n...`. Folder drops additionally offer "Set as working directory" on the chip; choosing it updates the session working dir (goosed session/agent API) and the context pill.
- Default context folder: config value (default `%USERPROFILE%\Documents\Goose`, created if missing); new sessions start there. (Settings UI in Phase 5; wizard sets it in Phase 7.)
- Context pill made live: reflects working-dir changes from drops and session resumes.
- Session history: `main` window gets a left sidebar — `SessionList` from goosed's session list route (title/description, working dir, updated-at, provider/model if available), client-side search box filtering by title + working dir. Click = resume: fetch full conversation, rebuild message list, set working dir pill. Delete with confirm. Overlay gets a lightweight recent-sessions dropdown (last 10) on the session button.
- Artifacts sidepane (`main` window, right side, collapsible): derive artifacts by scanning tool-call events for file-writing tools (developer extension write/edit tools — match on tool name patterns recorded in `docs/VERSIONS.md`) and collecting the target paths. Each `ArtifactCard`: filename, path, tool that produced it, timestamp; actions: Open (shell open), Show in Folder, Copy Path. Rebuilt on session resume by replaying the loaded conversation's tool calls. Persist nothing app-side.

Acceptance: drop three files + a folder, set the folder as working dir, agent reads them; ask agent to create a file and it appears in the artifacts pane and opens on click; quit, relaunch, resume the session from history — conversation, working dir, and artifacts all restored.

## Phase 5 — Settings panel, providers, Ollama management

Scope:
- Settings window IA: sidebar sections — General, Providers, Ollama Models, Extensions, Notifications, Appearance, Advanced (collapsed), Setup & Repair. Support deep links: `open_settings(section: string, highlight?: string)` command; "Fix this" buttons from Phase 1 now target sections (e.g. `provider_unreachable` → Providers with the active profile highlighted via a temporary outline animation).
- General: default context folder picker, approval mode, auto-summarize threshold, telemetry/auto-update toggles (read/write through goosed config routes where they're Goose settings; app config otherwise).
- Providers (`config/providers.rs`):
  - Profile model: `{ id, name, provider_type (ollama|openrouter|anthropic|openai|custom_openai|...), base_url, models: string[], network_tier (computed: local | personal | remote), tools_enabled: bool, created_at }`. Secrets via `keyring` only.
  - `network_tier` computation: `local` = host in {localhost, 127.0.0.1, ::1}; `personal` = host in Tailscale CGNAT range `100.64.0.0/10` or hostname ends in `.ts.net`; `remote` = anything else (including plain RFC1918 LAN — treat it like remote unless it's a Tailscale address). Badge labels per tier: "🖥 local", "🔒 private network", "☁ remote".
  - `tools_enabled`: per-profile toggle; when off, the profile is chat-only and can never be selected as a session's tool-calling provider (see Phase 9).
  - CRUD UI with per-type forms; tier badge on every row; adding a `remote`-tier profile shows the explicit privacy warning dialog (must click "I understand") before save. `personal`-tier profiles show the lighter badge only, no blocking dialog — still visibly distinct from `local` since they can go offline (see Phase 9).
  - "Activate for current session" and "Set as default" actions. Activation writes provider/model through goosed config routes.
  - **Context handoff gate**: switching an *existing session with any history or file chips* from a `local` profile to a `remote`-tier profile opens a blocking modal: "Keep context (send it to <host>)" vs "Start clean". No remember-choice checkbox — by design, every time. "Start clean" forks/creates a new session on the remote profile and leaves the old session intact. Switching to a `personal`-tier profile does **not** trigger this gate.
  - Network-tier indicator: when the active session's provider is `personal` or `remote`, chat UI shows a persistent badge next to the model name using that tier's label ("🔒 private network: <host>" / "☁ remote: <host>").
  - Optional strict mode (config toggle): disable file/folder drop while a `remote`-tier provider is active (drop zone shows why). Does not apply to `personal`-tier.
- Ollama Models section: installed list (`/api/tags`: name, size human-readable, modified); Pull field (model name input → `/api/pull` with progress bar per download, driven by `ollama://pull-progress`; support multiple concurrent pulls keyed by `pull_id`); Delete with confirm; "Browse models" button → `shell.open("https://ollama.com/library")`.
- Extensions section: list Goose extensions with enable/disable toggles; "Add custom" form (stdio: name/command/args/env; http: name/url) writing through goosed config routes.
- Advanced (collapsed `<details>`-style section):
  - Ollama env helper (`config/env_helper.rs`): read/write **user-level** environment variables via `HKCU\Environment` registry (`OLLAMA_HOST`, `OLLAMA_MODELS`, `OLLAMA_NUM_PARALLEL`, `OLLAMA_KEEP_ALIVE`, `OLLAMA_CONTEXT_LENGTH`); after a change, offer "Restart Ollama now" (only if we own the process; otherwise instruct the user). Show current effective values.
  - Model parameters: temperature, top_p, context length, planner provider/model — written to Goose config (`GOOSE_TEMPERATURE` etc. via config routes).
- Notifications section: per-event toggles from Phase 3.

Acceptance: create an OpenRouter profile (key retrievable only via keyring, absent from config.json on disk), switch a session with history to it and get the keep/jettison modal both times you try it; pull a small model with visible progress; toggle an extension and see it reflected in a new session; break the provider (bad key) and confirm "Fix this" lands on the highlighted profile.

## Phase 6 — Hotkey: Copilot key + theming

Scope:
- Copilot key (`hotkey.rs`): the key typically emits `Win+Shift+F23` (or is firmware-mapped). Implement a low-level keyboard hook (`SetWindowsHookExW` with `WH_KEYBOARD_LL` via the `windows` crate, running on a dedicated thread) that detects the chord, swallows it, and toggles the overlay. Feature-flag it behind a setting ("Use Copilot key"), default ON when the chord is ever observed, otherwise fall back to the configured accelerator. Document the PowerToys remap fallback in Settings help text. Keep the standard `tauri-plugin-global-shortcut` path for user-chosen hotkeys, with a "record hotkey" capture UI in Settings → General.
- Theming: theme = one CSS file defining custom properties (`--bg`, `--surface`, `--text`, `--text-muted`, `--accent`, `--border`, `--radius`, `--font-family`, `--overlay-opacity`) — document the full contract in `themes/README.md`. Built-ins: default, dark. User themes: any `.css` dropped in `%APPDATA%/goose-overlay/themes/` appears in Appearance; loaded at runtime by injecting a `<link>`. Background image: Appearance file-picker stores a path in app config; applied as an inline style on the window root with a dim/blur slider (`--bg-image-dim`). Appearance also holds overlay size/position prefs (remember-last-position toggle).

Acceptance: on a machine with a Copilot key, the key summons the overlay and Windows Copilot does not launch; on any machine, a custom recorded hotkey works after restart; dropping a valid custom CSS theme restyles all windows without rebuild; background image + dim renders in overlay and main.

## Phase 7 — First-run wizard, repair, installer

Scope:
- Wizard window (also reachable from Settings → Setup & Repair), steps:
  1. **Detect**: check for Ollama (binary on PATH / default install dir / port probe) and Goose (`goosed` binary in known install locations / PATH). Show found versions.
  2. **Install missing**: download official Windows installers over HTTPS (URLs + expected patterns in `docs/VERSIONS.md`), verify size/hash where published, run them (Ollama supports silent `/S`-style install — verify against pinned version; otherwise hand off to installer UI and re-detect on completion). Elevation via the installer's own UAC prompt is fine.
  3. **Configure**: Ollama endpoint (default localhost:11434, editable), default provider (local Ollama preselected), default context folder (default `Documents\Goose`, create it), hotkey (Copilot key preselected if the hook has observed it, else `Alt+Space`).
  4. **First model**: curated list of ≤4B models defined in `src/lib/starter_models.ts` — each entry `{ tag, label, blurb, size_gb }` (populate with 3–5 current small instruct models; verify tags exist on ollama.com at implementation time and record in `docs/VERSIONS.md`). Radio select → pull with progress → verify via `/api/tags`.
  5. **Done**: start the stack, open the overlay with a canned first prompt suggestion.
- First-launch detection: config flag `setup_completed`; if false, show wizard instead of overlay.
- Repair mode: same wizard launched with a `mode=repair` param — pre-runs detection, highlights whichever step is broken (maps from stack status), lets the user jump straight to it.
- NSIS installer for our app itself (Tauri bundler): installs the app + autostart-on-login option (registry Run key, toggle in Settings → General); does **not** bundle Ollama/Goose (wizard handles those at first run).

Acceptance: on a clean Windows VM with nothing installed, running our installer then first launch walks through installing Ollama + Goose, pulls a starter model with progress, and lands in a working chat; deleting the model then relaunching routes the degraded state to repair, which fixes it.

## Phase 8 — Hardening & polish

Scope:
- Session insights on the history view (total sessions/tokens) if the goosed insights route is available in the pinned version.
- Concurrency: block sending while a reply streams (or implement cancel via goosed's cancel route if present); handle overlay hide mid-stream (stream continues, notification on completion per Phase 3).
- Long-transcript performance: virtualize the message list (`@tanstack/react-virtual`) beyond ~200 messages.
- Graceful goosed restart preserving the active session (resume by id after restart).
- Audit: no secret in any log line, config.json, or frontend memory (grep + manual review); event payload size caps for huge tool outputs (truncate in Rust with "expand" fetching full result on demand).
- Accessibility pass: keyboard-only operation of overlay (tab order, Escape, Enter-to-approve is NOT allowed — approval buttons require explicit click or Space when focused), reduced-motion respect.
- Write `docs/RELEASE.md`: build, sign (placeholder), version-bump checklist including re-verifying goosed API paths against the pinned version.

Acceptance: 30-minute soak with repeated summon/dismiss during active streams shows no leaks/orphans; a 500+ message session scrolls smoothly; secret audit passes.

## Phase 9 — Thought-partner (chat-only) mode

**REMOVED.** The chat/agentic distinction this phase introduced was taken out
again — see core rule 6. Everything below is the original plan, kept for the
record; `tools_enabled` no longer exists on a provider profile either (it was
dropped earlier, in favor of the per-session toggle that has now also gone).
The parts that survive are per-message branching, regenerate, copy-as-markdown,
the pasted-document chip, and the per-provider connectivity state.

Adds support for a second kind of provider: a personal, tool-free LLM (e.g. a self-hosted model reached over Tailscale) used for critique, planning, and discussion rather than agentic work. Builds on the `network_tier` and `tools_enabled` fields added to the provider profile in Phase 5.

Scope:
- **Chat-only UI mode**: when the active session's provider has `tools_enabled: false`, hide all agent chrome — no tool-approval badge, no `ToolCallCard` rendering path, no context pill (unless a document was explicitly attached; see below). The composer and message list are otherwise identical to agentic mode.
- **Overlay → full window auto-promotion**: for a `tools_enabled: false` profile, the *first message sent from the overlay* in a new session automatically opens the `main` window bound to that session and hides the overlay, rather than continuing to stream inline in the compact view. This only fires on the first message of a session, never mid-conversation. Agentic-mode sessions keep today's manual "expand" behavior.
- **Long-form input handling**: since there's no filesystem tool to hand a path to, dropped files or large pastes (>500 chars, configurable threshold) must have their **content read and inlined** into the message rather than referenced by path. Implement:
  - Paste detection in the composer: a paste over the threshold collapses into a `PastedDocumentChip` (filename-less, labeled "Pasted text — N words", expandable preview) instead of filling the input box; the full text is sent as inlined content in the message.
  - File drop on a `tools_enabled: false` session: read the file content in Rust (text files only; reject/warn on binaries) and attach it the same way as a paste, rather than sending a path reference. Size-cap with a clear error if exceeded (start at a configurable ~200KB).
  - This is a genuinely different code path from the Phase 4 drag-drop flow (which sends paths for the filesystem tool to read) — branch on `tools_enabled` at the composer/drop-handler level, not by trying to unify them.
- **No system-prompt presets.** Explicitly out of scope — don't build a preset picker for this mode.
- **Message-level branching**: hovering/focusing any message shows a "Branch from here" action. It calls goosed's session fork-with-truncation capability (already used at the session level in Phase 5/8) scoped to that message's timestamp, creating a new session that shares history up to that point and diverges after. The branched session appears in session history as its own entry, with a note of which session/message it branched from (store this as session metadata if goosed exposes a free-form metadata field; otherwise encode it in the auto-generated title, e.g. "Branch of <original title> @ msg N").
- **Regenerate**: every assistant message gets a "Regenerate" action that re-submits the preceding user message and streams a new assistant response. Implementation choice: since goosed sessions are append-only conversation logs, treat regenerate as branch-from-the-prior-user-message + immediately resend, so the original response isn't lost (visible via message-level branch history) rather than mutating in place. Applies in both chat-only and agentic modes.
- **Connectivity resilience for `personal`/`remote` tiers**: add a per-provider (not just per-stack) health state distinct from the local Ollama/goosed health machine in Phase 1. Ping the provider's base URL on a lighter interval (e.g. every 15s only when a session on that provider is active, to avoid constantly pinging a Tailscale node) and surface a specific `provider_unreachable_offline` state — copy should read like "can't reach <host> — check Tailscale" rather than the generic provider-error message used for bad keys/config.
- **Reading-friendly rendering pass**: for chat-only sessions, default to a wider content column and slightly larger line-height (still theme-driven via custom properties — add `--reading-max-width` and `--reading-line-height` to the theme contract, applied when `tools_enabled: false`). No new theme file required, just conditional class application.
- **Copy as Markdown**: every assistant message gets a "Copy as Markdown" action (copies the raw markdown source, not rendered HTML) — useful in both modes but especially for pulling critique/planning output into other documents.

Acceptance: configure a Tailscale-reachable Ollama-compatible endpoint as a `tools_enabled: false`, `personal`-tier profile; sending a first message from the overlay pops the full window automatically; pasting a 2000-word draft collapses to a chip and the model can discuss its full content; branching from a mid-conversation message produces a separate, independently continuable session; regenerating a response keeps the original reachable via branch history; disconnecting Tailscale surfaces the offline-specific message within 15s and recovers automatically on reconnect.

## Phase 10 — Reasoning visibility

Applies to **both** agentic and chat-only sessions, for any provider/model that emits reasoning/thinking content (reasoning-capable local models via Ollama's `think` parameter, and any goosed-mediated equivalent — feature-detect per model rather than assuming support).

Scope:
- **Feature detection**: when starting a session, check whether the active model is known to support a thinking/reasoning mode. Maintain a small table in `src/lib/reasoning_models.ts` (model name patterns → supports-reasoning: bool) rather than guessing from provider type alone, since this varies per model, not per provider. Record findings in `docs/VERSIONS.md`.
- **Thinking indicator**: while a response is streaming and no visible text has arrived yet (or between reasoning and final answer), show a distinct `ThinkingIndicator` (animated, e.g. "Thinking…") in place of the usual typing indicator, so it's visually clear the model is doing reasoning work rather than just being slow.
- **Reasoning panel**: if the model/provider streams distinct reasoning content (separate from the final answer — verify whether the pinned goosed version surfaces a distinct reasoning/thought event type in its streaming reply, or whether reasoning arrives as part of the same text stream and needs to be split on think-tags), render it in a collapsible `ReasoningPanel` above the final answer, streamed live, visually de-emphasized (muted color, smaller text) relative to the final response. Default collapsed-after-completion, expandable at any time; live-streaming reasoning while it's arriving should auto-expand so the user can watch it, then collapse once the final answer starts if the user hasn't manually pinned it open.
- Both agentic and chat-only tool cards / reasoning panels can co-exist in a single message (a tool-calling session can still show reasoning before/around a tool call) — treat reasoning as a message-level stream segment, independent of tool-call segments.

Acceptance: with a reasoning-capable model configured, the thinking indicator appears immediately on send, the reasoning panel streams live and auto-expands, then settles to collapsed once the final answer is complete; a user can re-expand it at any point later while scrolling history; a non-reasoning model shows the normal typing indicator and no reasoning panel at all.

## Phase 11 — ChatML export with reasoning traces

Scope:
- **Export scope**: available at the session level (Session History → row action "Export") and inline (a message's context menu → "Export from here", producing an export truncated to that point).
- **Format**: write a `.chatml` (plain UTF-8 text) file using standard ChatML role delimiters:
  ```
  <|im_start|>system
  <system prompt text, if any><|im_end|>
  <|im_start|>user
  <message text><|im_end|>
  <|im_start|>assistant
  <think>
  <reasoning trace text, if any>
  </think>
  <final assistant response text><|im_end|>
  ```
  Reasoning traces are wrapped in `<think>...</think>` inside the assistant turn (matching the convention used by current open reasoning models) and omitted entirely for turns with no reasoning content — don't emit empty `<think>` blocks.
- **Sidecar metadata file**: ChatML has no native representation for tool calls, attachments, or session metadata, so write a companion `<same-name>.meta.json` alongside the `.chatml` file containing: session id, provider/model per turn (models can change mid-session), working directory, timestamps, and a structured list of tool calls (name, params, result) keyed by their position in the turn sequence, so the two files can be losslessly cross-referenced without polluting the ChatML file itself.
- **Tool calls in the ChatML body**: represent them minimally inline as a fenced note so the plain-text file is still readable on its own, e.g. a line like `[tool_call: shell → see meta.json#tool_calls[2]]` immediately before/after the relevant assistant turn — full detail lives in the sidecar, not duplicated in both places.
- **UI**: export triggers a native save dialog (`tauri-plugin-dialog`) defaulting to the session's title as filename, saved wherever the user chooses (not forced into the context folder).
- **No import path required for v1** — this is one-way, for archiving/sharing/fine-tuning-data purposes. Note re-import as a backlog item if it comes up later.

Acceptance: exporting a session that mixes reasoning, tool calls, and plain turns produces a `.chatml` file that reads cleanly as plain text and a `.meta.json` that fully reconstructs tool call details and per-turn model info; exporting "from here" on a mid-conversation message produces a correctly truncated pair of files; a session with no reasoning content produces a `.chatml` with no stray `<think>` tags.

---

## Known risks & decided fallbacks

- **A hotkey can be taken by another app** (Alt+Space and PowerToys) → registration failure is reported in the hub with a "Choose another hotkey" link; never block startup on it.
- **Engine API drift** → every path lives in `src-tauri/src/bigtiny/` and the attach code; Kitty refuses an engine older than API version 2 and says why; `docs/VERSIONS.md` records the contract.
- **The engine is shared with other apps** → Kitty never kills it, never changes another app's data, and restarts it only when no other app is using it (or the user says "Restart anyway").
- **Artifacts detection is heuristic** (tool-name matching) → acceptable; false negatives are fine, never fabricate entries.
- **Reasoning/thinking support varies per model, not per provider** → never assume; feature-detect via `reasoning_models.ts` and fall back to the plain typing indicator with no reasoning panel if unsupported or undetected.
- **`personal`-tier (Tailscale) endpoints can legitimately go offline** → this is expected, not an error state; keep its health messaging distinct from misconfiguration ("check Tailscale" vs. "check your API key"), and recover silently on reconnect without user action.

## Definition of done for any task

Compiles (`cargo build` + `pnpm build`), lints clean, relevant tests pass, works in a manual run on Windows, no TODOs left in code (open a `docs/BACKLOG.md` item instead), commit message references the phase.
