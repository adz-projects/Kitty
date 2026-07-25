# Architecture

One-page module map with dependency direction — the flat inventory in
`PROJECT_INDEX.md` has no edges; this does. Arrows read "depends on" /
"calls into."

## Rust (`src-tauri/src/`)

```
lib.rs (app setup, window creation, generate_handler! list)
  │
  ├─► windows.rs, tray.rs, hotkey.rs        (chrome: windows/tray/global shortcut)
  │
  ├─► lifecycle/                             (process supervision)
  │     mod.rs          orchestrates start_stack/shutdown
  │       ├─► ollama_proc.rs                 probe/spawn Ollama
  │       ├─► bigtiny_proc.rs                probe/spawn the BigTiny daemon
  │       ├─► adaptive_pathway_proc.rs       probe/spawn the AP sidecar
  │       ├─► scheduler.rs                   fires due scheduled tasks
  │       ├─► health.rs                      the two 5s health loops
  │       └─► embedding.rs                   embedding-model convergence
  │
  ├─► bigtiny/                                (REST/SSE client — the only
  │     client.rs   BigTinyClient: base URL + X-API-Key + JSON helpers   chat backend)
  │     sessions.rs session CRUD over REST, replayed as chat://* events on load
  │     stream.rs   POST .../send SSE consumption -> chat://* events,
  │                  + the adaptive-pathway record_outcome backstop
  │     providers.rs sync Kitty's active provider profile into BigTiny's registry
  │     mcp.rs       MCP server CRUD + ensure_builtin_servers (replacement-mcp,
  │                  adaptive-pathway, wasm-math-mcp, brave-mcp-search) self-heal
  │
  ├─► config/                                (app config, %APPDATA%/Kitty/config.json)
  │     mod.rs         Config struct, load/save, bundled_plugin_path()
  │     providers/     provider profiles: network tier, keyring, connection
  │                     test (mod/network/keyring/connection.rs)
  │     recipes.rs, recipe_yaml.rs, scheduled_tasks.rs
  │
  ├─► adaptive_pathway/mod.rs                 plain HTTP client for the AP
  │                                           sidecar (see docs/ADAPTIVE_PATHWAY.md)
  ├─► ollama/mod.rs, openrouter/mod.rs        provider-specific HTTP clients
  │
  ├─► commands/                               #[tauri::command] handlers —
  │     session/       new/send/cancel/load/fork/delete, mode, thinking effort
  │     provider.rs, adaptive_pathway.rs, mcp_servers.rs, recipes.rs,
  │     scheduled_tasks.rs, folders.rs, ollama.rs, file.rs,
  │     window.rs, setup.rs, config.rs, logs.rs
  │     (thin wrappers over the modules above — no business logic of their own)
  │
  ├─► wizard.rs                               first-run Ollama detect/install/configure
  ├─► notifications.rs                        Windows toast + tray pending state
  ├─► log_capture.rs                          in-memory ring buffer for Settings' log view
  └─► util.rs                                 shared http_client(), hidden_command()

state.rs        AppState (managed Tauri state): config, bigtiny/ollama/AP
                ManagedProcess handles, StackStatus, in-flight session ids.
                Everything above reads/writes through this.
```

Dependency direction is meant to be roughly top-to-bottom: `commands/` calls
into `lifecycle/`/`config/`/`bigtiny/`, never the reverse, with one
deliberate, narrow exception — `lifecycle/scheduler.rs` calls
`commands::new_session`/`send_prompt` to fire a due scheduled task headlessly.
That reverse edge is confined to that one file rather than spread through
`lifecycle/mod.rs` (see that file's own doc comment).

## Frontend (`src/`)

```
windows/{overlay,main,settings,wizard}/App.tsx   one entry point per Tauri window label
  │
  ├─► components/chat/       Composer, MessageList/MessageItem, ThinkingBox,
  │                          ApprovalPrompt, ToolCallCard, HintBadge — shared
  │                          verbatim between overlay and main (CLAUDE.md rule 5)
  ├─► components/sessions/   SessionList (main only), RecentSessions (overlay)
  ├─► components/settings/   one panel per Settings sidebar section
  ├─► components/artifacts/  ArtifactsPane (main only)
  │
  ├─► stores/                zustand stores — render state only, never the
  │     chatStore.ts           source of truth (that's BigTiny, CLAUDE.md rule 3)
  │       chat/                 extracted pure helpers (types, message/loop/
  │                             approval/error utils, mode-info cache) —
  │                             re-exported from chatStore.ts so every existing
  │                             import path keeps working unchanged
  │     sessionStore.ts, adaptivePathwayStore.ts, stackStore.ts
  │
  └─► lib/
        ipc.ts        the ONLY file that calls invoke() — typed wrappers
                      around every Tauri command, plus Tauri event listeners
        types.ts      TS mirrors of Rust structs (kept in sync by hand)
        recipes.ts, chatml.ts, provider_trust.tsx, system_prompts.ts, ...
```

`lib/ipc.ts` is the chokepoint CLAUDE.md's "webview never fetches localhost
directly" rule depends on — every component goes through it, never `invoke()`
directly. It's also backend-agnostic by design: `src-tauri/src/bigtiny/`
emits the same `chat://*`/`session://*` event shapes and command return types
a frontend written against goosed's ACP surface already expected, so this
layer needed zero changes when the backend swapped from goosed to BigTiny.

## Plugins (`plugins/`)

See `docs/PLUGINS.md` for the full pattern. Independent Python packages
(`adaptive-pathway`, `replacement-mcp`, `wasm-math-mcp`, `brave-mcp-search`)
plus the BigTiny daemon itself (vendored in-tree at `plugins/bigtiny/`) are
all frozen to `.exe`s via PyInstaller and bundled through Tauri's
`externalBin` — `python plugins/build.py` builds all six targets (`bigtiny`,
`adaptive-pathway`, `adaptive-pathway-mcp`, `replacement-mcp`,
`wasm-math-mcp`, `brave-mcp-search`). `adaptive-pathway`'s HTTP sidecar and
the BigTiny daemon itself are both Kitty-managed processes (`ManagedProcess`,
probed then spawned); `replacement-mcp`, `adaptive-pathway-mcp`,
`wasm-math-mcp`, and `brave-mcp-search` are stdio MCP servers registered with
BigTiny's own `/api/mcp/servers` registry (`bigtiny::mcp::ensure_builtin_servers`),
not spawned directly by Kitty. `wasm-math-mcp` is on by default (no
credentials); `brave-mcp-search` is off by default and needs a Brave Search
API key, stored in the keyring rather than `config.json` — disabling it
always deletes the stored key, so re-enabling always requires re-entering it.

## Cross-cutting: the three "who's the source of truth" boundaries

1. **Session/conversation state** → BigTiny. Frontend `messages[]` is a
   reconstruction from `chat://*` events, never persisted app-side.
2. **MCP server registrations** (which tools every session has access to)
   → BigTiny's own `/api/mcp/servers`, not Kitty's `config.json` — Kitty only
   self-heals the two bundled servers' command paths/enabled state into it.
3. **Secrets** → Windows Credential Manager (`keyring`, service `kitty`),
   never `config.json`, never a frontend variable.
