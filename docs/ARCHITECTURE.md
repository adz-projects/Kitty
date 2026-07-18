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
  │       ├─► goosed.rs                      probe/spawn `goose serve`
  │       ├─► adaptive_pathway_proc.rs       probe/spawn the AP sidecar
  │       ├─► conflict.rs                    detect stock Goose Desktop
  │       ├─► scheduler.rs                   fires due scheduled tasks
  │       ├─► health.rs                      the two 5s health loops
  │       └─► embedding.rs                   embedding-model convergence
  │
  ├─► goosed/                                (ACP client — the version-drift
  │     api.rs      AcpClient, request/respond/notify        isolation boundary)
  │     stream.rs   incoming-frame dispatch -> chat://* events
  │     types.rs    shared bookkeeping types (Pending/Perm/Activity/ToolCalls)
  │
  ├─► config/                                (app config, %APPDATA%/.../config.json)
  │     mod.rs         Config struct, load/save, bundled_plugin_path()
  │     providers/     provider profiles: network tier, keyring, connection
  │                     test, goosed env assembly (mod/network/keyring/
  │                     connection/env.rs)
  │     recipes.rs, recipe_yaml.rs, scheduled_tasks.rs
  │
  ├─► goose_config.rs                        reads/writes GOOSE's config.yaml
  │                                           (extension defaults registry —
  │                                            shared with Goose Desktop)
  │
  ├─► adaptive_pathway/mod.rs                 plain HTTP client for the AP
  │                                           sidecar (see docs/ADAPTIVE_PATHWAY.md)
  ├─► ollama/mod.rs, openrouter/mod.rs        provider-specific HTTP clients
  │
  ├─► commands/                               #[tauri::command] handlers —
  │     session/       new/send/cancel/load/fork/delete, mode, thinking effort
  │     provider.rs, adaptive_pathway.rs, replacement_mcp.rs, recipes.rs,
  │     scheduled_tasks.rs, folders.rs, extensions.rs, ollama.rs, file.rs,
  │     window.rs, setup.rs, config.rs, logs.rs
  │     (thin wrappers over the modules above — no business logic of their own)
  │
  ├─► wizard.rs                               first-run detect/install/configure
  ├─► notifications.rs                        Windows toast + tray pending state
  ├─► log_capture.rs                          in-memory ring buffer for Settings' log view
  └─► util.rs                                 shared http_client(), hidden_command()

state.rs        AppState (managed Tauri state): config, goosed/ollama/AP
                ManagedProcess handles, StackStatus, the live AcpClient,
                in-flight session ids. Everything above reads/writes through this.
```

Dependency direction is meant to be roughly top-to-bottom: `commands/` calls
into `lifecycle/`/`config/`/`goosed/`, never the reverse, with one
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
  │     chatStore.ts           source of truth (that's goosed, CLAUDE.md rule 3)
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
directly.

## Plugins (`plugins/`)

See `docs/PLUGINS.md` for the full pattern. In one line: two independent
Python packages, frozen to `.exe`s via PyInstaller, bundled through Tauri's
`externalBin`, with their Rust-side wiring following one of two shapes
depending on who spawns the process (Kitty, for a sidecar; goosed, for an MCP
extension).

## Cross-cutting: the three "who's the source of truth" boundaries

1. **Session/conversation state** → goosed. Frontend `messages[]` is a
   reconstruction from `chat://*` events, never persisted app-side.
2. **Extension defaults** (which extensions every new session starts with)
   → goose's own `config.yaml`, not Kitty's `config.json`.
3. **Secrets** → Windows Credential Manager (`keyring`), never `config.json`,
   never a frontend variable.
