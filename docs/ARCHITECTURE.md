# Architecture

One-page module map with dependency direction — the flat inventory in
a flat file inventory has no edges; this does. Arrows read "depends on" /
"calls into."

## Rust (`src-tauri/src/`)

```
lib.rs (app setup, window creation, generate_handler! list)
  │
  ├─► windows.rs, tray.rs, hotkey.rs        (chrome: windows/tray/global shortcut)
  │     screenshot.rs                        Win32 GDI region capture  [desktop only]
  │
  ├─► lifecycle/                             (process supervision)
  │     mod.rs          orchestrates start_stack/shutdown
  │       ├─► bigtiny_proc.rs                probe/spawn the BigTiny daemon  [desktop]
  │       ├─► bigtiny_embedded.rs            host the daemon in-process      [Android]
  │       ├─► bigtiny_env.rs                 the env contract both of the above pass
  │       ├─► scheduler.rs                   fires due scheduled tasks
  │       ├─► health.rs                      the 5s health loop
  │       ├─► embedding.rs                   embedding-model convergence
  │       └─► engine_restart.rs              queue a local-engine reload until idle
  │
  ├─► bigtiny/                                (REST/SSE client — the only
  │     client.rs   BigTinyClient: base URL + X-API-Key + JSON helpers   chat backend)
  │     sessions.rs session CRUD over REST, replayed as chat://* events on load
  │     stream.rs   POST .../send SSE consumption -> chat://* events,
  │                  + the adaptive-pathway record_outcome backstop
  │     providers.rs sync Kitty's active provider profile into BigTiny's registry
  │     pathway.rs  adaptive-pathway belief browser / graph health / domains
  │     mcp.rs       MCP server CRUD + ensure_builtin_servers (kitty-tools,
  │                  kitty-web, kitty-wasm) self-heal. Bundled exe on desktop,
  │                  transport "in_process" on Android (no exec()).
  │
  ├─► config/                                (app config, %APPDATA%/Kitty/config.json;
  │     mod.rs         Config struct, load/save,     app-private dir on Android)
  │                     bundled_plugin_path(), models_dir()
  │     providers/     provider profiles: network tier, keyring, endpoint
  │                     scheme probing, connection test
  │     recipes.rs, recipe_yaml.rs, scheduled_tasks.rs
  │
  ├─► models/                                 GGUF acquisition (no AppHandle —
  │     download.rs  resumable HuggingFace fetch: .part + sha256 + atomic rename
  │     gguf.rs      minimal header read for the model card
  │
  ├─► openrouter/mod.rs                       provider-specific HTTP client
  │
  ├─► commands/                               #[tauri::command] handlers —
  │     session/       new/send/cancel/load/fork/delete, mode, thinking effort
  │     provider.rs, adaptive_pathway.rs, memory.rs, mcp_servers.rs,
  │     recipes.rs, scheduled_tasks.rs, folders.rs, models.rs, file.rs,
  │     screenshot.rs, window.rs, setup.rs, config.rs, logs.rs
  │     (thin wrappers over the modules above — no business logic of their own)
  │
  ├─► wizard.rs                               first-run detect/configure + autostart
  ├─► notifications.rs                        toast + tray pending state
  ├─► log_capture.rs                          in-memory ring buffer for Settings' log view
  └─► util.rs                                 shared http_client(), hidden_command()

state.rs        AppState (managed Tauri state): config, the BigTiny
                ManagedProcess handle, StackStatus, in-flight session ids.
                Everything above reads/writes through this.
```

**There is no Ollama module.** Kitty manages no inference process at all: the
local engine is **LiteRT** linked into the daemon
(`plugins/bigtiny_rust/src/litert/`) — embeddings on both platforms, plus
generative compaction summarization on Windows only. There is no local chat.
`provider_type: "ollama"` survives
only as a *remote* endpoint dialect the user points at a server they run
themselves. `src-tauri/src/ollama/`, `commands/ollama.rs`,
`lifecycle/ollama_proc.rs` and `config/env_helper.rs` were deleted in Phase 2b.

**Platform split.** Desktop spawns `bigtiny-daemon.exe` as a child process;
Android hosts the identical daemon in-process via `bigtiny_rust::run`, because
Android 10+ will not `exec()` a binary out of app-writable storage. Both go
through the same HTTP boundary and the same env contract, so nothing above
`lifecycle/` knows which one it is talking to.

Dependency direction is meant to be roughly top-to-bottom: `commands/` calls
into `lifecycle/`/`config/`/`bigtiny/`, never the reverse, with one
deliberate, narrow exception — `lifecycle/scheduler.rs` calls
`commands::new_session`/`send_prompt` to fire a due scheduled task headlessly.
That reverse edge is confined to that one file rather than spread through
`lifecycle/mod.rs` (see that file's own doc comment).

## Frontend (`src/`)

```
windows/{hub,overlay,screenshot-select}/App.tsx  one entry point per window label
  │   hub = chat + saved chats + settings + wizard, routed by `routeStore`
  │         (one window, four views — Android's whole UI, and desktop's
  │          full window; `overlay` and `screenshot-select` are desktop-only)
  │
  ├─► components/hub/        ChatWorkspace (the three-column desktop shell,
  │                          one column + a bottom tab bar on Android),
  │                          MobileTabBar
  ├─► components/chat/       Composer, MessageList/MessageItem, ThinkingBox,
  │                          ApprovalPrompt, ToolCallCard, ChatHeaderControls —
  │                          shared verbatim between overlay and hub (rule 5)
  ├─► components/sessions/   SessionList — the chat sidebar on desktop, the
  │                          "Saved Chats" tab on Android
  ├─► components/settings/   one panel per Settings sidebar section
  ├─► components/artifacts/  ArtifactsPane — third column on desktop, a sheet
  │                          over the conversation on Android
  ├─► components/wizard/     first-run steps (a different set per platform)
  │
  ├─► stores/                zustand stores — render state only, never the
  │     chatStore.ts           source of truth (that's BigTiny, CLAUDE.md rule 3)
  │       chat/                 extracted pure helpers (types, message/loop/
  │                             approval/error utils, mode-info cache) —
  │                             re-exported from chatStore.ts so every existing
  │                             import path keeps working unchanged
  │     sessionStore.ts, adaptivePathwayStore.ts, stackStore.ts,
  │     routeStore.ts (which hub view is showing)
  │
  └─► lib/
        ipc.ts        the ONLY file that calls invoke() — typed wrappers
                      around every Tauri command, plus Tauri event listeners
        types.ts      TS mirrors of Rust structs (kept in sync by hand)
        platform.ts   isAndroid() + the `data-platform` attribute CSS keys off
        viewport.ts   pins the app box to the visual viewport (soft keyboard)
        recipes.ts, chatml.ts, provider_trust.tsx, system_prompts.ts, ...
```

`lib/ipc.ts` is the chokepoint CLAUDE.md's "webview never fetches localhost
directly" rule depends on — every component goes through it, never `invoke()`
directly. It's also backend-agnostic by design: `src-tauri/src/bigtiny/`
emits the same `chat://*`/`session://*` event shapes and command return types
a frontend written against goosed's ACP surface already expected, so this
layer needed zero changes when the backend swapped from goosed to BigTiny.

## Plugins (`plugins/`)

See `docs/PLUGINS.md` for the full pattern. `kitty-tools`, `kitty-web` and
`kitty-wasm` (all Rust) plus the BigTiny daemon itself
(`plugins/bigtiny_rust/`) are built with `cargo build --release` and bundled
through Tauri's `externalBin` — `python plugins/build.py` builds all four
targets. The behavioral-memory engine (`plugins/adaptive-pathway_rust`) is not
a target of its own: it is a path dependency statically linked into the
daemon.

**On Android none of that applies.** `externalBin` is cleared
(`tauri.android.conf.json`), the daemon is hosted in-process, and the three MCP
servers register with `transport: "in_process"` — Android 10+ will not
`exec()` a binary in app-writable storage, so there is nothing a bundled
sidecar could be. `plugins/build.py` is the desktop lane only and has no
Android triple by design.

The BigTiny daemon is a Kitty-managed process on desktop (`ManagedProcess`,
probed then spawned); `kitty-tools`, `kitty-web` and `kitty-wasm` are stdio MCP
servers registered with BigTiny's own `/api/mcp/servers` registry
(`bigtiny::mcp::ensure_builtin_servers`), not spawned directly by Kitty.
`kitty-tools`, `kitty-web`, and `kitty-wasm` are on by default (no
credentials). `kitty-tools` hosts 21 tools in one process — the always-on
shell/workspace/file/word/cache/scratchpad set, plus read-only Excel/PDF
tools, plus 4 visualization tools (accessible table, SVG diagram, chart,
Mermaid) gated by their own Settings toggle (an env var on this one process,
not a separate server) — no network calls of its own. `kitty-web` hosts the merged,
count-tiered `lean_web_search`/`lean_web_search_read_chunk` and
`lean_web_scrape` (DuckDuckGo always available; Brave preferred per-query when
configured — Brave's toggle needs an API key stored in the keyring rather
than `config.json`; disabling it always deletes the stored key, so
re-enabling always requires re-entering it). `kitty-wasm` hosts the sandboxed
WebAssembly compute tools (Python via a bundled CPython wasm guest, plus
arbitrary WASI modules) with no network and no filesystem beyond explicit
mounts. `replacement-mcp`, `brave-mcp-search`, `visualizations`,
`kitty-docs-web` and `wasm-math-mcp` are retired and their source has been
**deleted** — the ports are verified and shipping, and git history holds the
originals if a behavioral question ever needs settling. Their server rows are
actively removed from the daemon on sync (`RETIRED_BUILTINS` in
`src-tauri/src/bigtiny/mcp.rs`).

## Cross-cutting: the three "who's the source of truth" boundaries

1. **Session/conversation state** → BigTiny. Frontend `messages[]` is a
   reconstruction from `chat://*` events, never persisted app-side.
2. **MCP server registrations** (which tools every session has access to)
   → BigTiny's own `/api/mcp/servers`, not Kitty's `config.json` — Kitty only
   self-heals the two bundled servers' command paths/enabled state into it.
3. **Secrets** → Windows Credential Manager (`keyring`, service `kitty`),
   never `config.json`, never a frontend variable. On Android the same
   contract is met by a different store — AES-256-GCM under a non-exportable
   AndroidKeyStore key (`src/android/secrets.rs` over `SecretStore.kt`),
   because `keyring` has no Android backend and degrades to an in-memory mock
   if you let it (D24).
