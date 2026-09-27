# Architecture

One-page module map with dependency direction. Arrows read "depends on" /
"calls into."

> ## One engine: BigTiny V2.
>
> - **`BigTinyV2/` is the engine Kitty runs, on both platforms.** `daemon/`
>   (the engine), `protocol/` (wire types shared with clients), `client/` (the
>   reusable Rust client Kitty links). It is multi-app: several frontends
>   (Kitty, a research pipeline, an AI notebook) attach to one instance, each
>   with its own sessions, providers, MCP servers, schedules and plugin
>   instances. It stays on the 2.x line (2.1.0 as of Kitty 1.0); changes made
>   for Kitty are additive and app-scoped, because other apps share it.
> - **`plugins/bigtiny_rust/` (V1) is frozen and unused.** Nothing builds or
>   links it; it is kept only as a rollback path. A V1 install's data is
>   brought across by Kitty's one-click import (`commands/v1_import.rs`).

## Rust (`src-tauri/src/`)

```
lib.rs (app setup, window creation, generate_handler! list;
  │     `kitty.exe --uninstall-cleanup` is handled first → uninstall.rs)
  │
  ├─► windows.rs, tray.rs, hotkey.rs        chrome: windows (overlay geometry per
  │     screenshot.rs                        monitor), tray states, global shortcuts,
  │                                           Win32 region capture  [desktop only]
  │
  ├─► lifecycle/                             finding and keeping the engine
  │     mod.rs            start_stack / attach / re-attach
  │       ├─► bigtiny_v2.rs       attach to (or spawn) the shared engine  [desktop]
  │       ├─► bigtiny_embedded.rs host the engine in-process              [Android]
  │       ├─► bigtiny_app_key.rs  register as app `kitty`; verify / reclaim the key
  │       ├─► bigtiny_env.rs      the start-up env both hosts pass (SpawnSnapshot)
  │       ├─► bigtiny_proc.rs     /api/health probe, per-launch registration token
  │       ├─► health.rs           status loop; re-attaches when the engine moved
  │       ├─► engine_restart.rs   safe restart (POST /api/admin/restart), queued
  │       │                        while other apps or Kitty's own turns are busy
  │       ├─► memory.rs           per-app memory plugins on/off (embedding model
  │       │                        on disk AND the user's toggle)
  │       ├─► embedding.rs        is the embedding model on disk
  │       └─► app_events.rs       the /api/apps/me/events listener: approvals,
  │                                schedule runs, session titles
  │
  ├─► approvals.rs                           which tool calls are answered
  │                                           automatically, the "always allow"
  │                                           scope, the pending list
  │
  ├─► bigtiny/                               REST/SSE client — the only chat backend
  │     client.rs      BigTinyClient: base URL + X-API-Key + JSON helpers
  │     sessions.rs    session CRUD, paging, search, replay as chat://* events
  │     stream.rs      POST .../send SSE → chat://* events (+ attach to a turn
  │                     running elsewhere), notices, tool results
  │     turn_text.rs   how a user turn is laid out (documents, file list) and
  │                     parsed back for replay
  │     providers.rs   sync every provider card into the engine
  │     specialists.rs, mcp.rs (bundled servers self-heal), pathway.rs,
  │     memorabilia.rs, effort.rs, vision.rs, context_window.rs
  │
  ├─► config/                                app config (%APPDATA%/Kitty/config.json;
  │     mod.rs         Config, load/save,      app-private dir on Android);
  │                     patch (merge patch)     every save emits config://changed
  │     providers/     provider cards: network tier, secrets, endpoint scheme
  │                     probing, connection test
  │
  ├─► models/                                helper-model downloads (no AppHandle):
  │     download.rs    resumable HuggingFace fetch: .part + sha256 + rename,
  │                     cancel, licence errors, unfinished downloads
  │     gguf.rs        minimal header read for the model card
  │
  ├─► openrouter/                            catalog (cost tier, tool support)
  │
  ├─► commands/                              #[tauri::command] handlers — thin
  │     session/ provider.rs export.rs scheduled_tasks.rs v1_import.rs
  │     models.rs mcp_servers.rs incoming.rs setup.rs ... (no business logic)
  │
  ├─► android/                               Kotlin bridge [Android only]: secrets,
  │                                           notifications, share/notification
  │                                           intents, foreground services, SAF
  ├─► notifications.rs                       toasts (a channel per event on
  │                                           Android) + tray state
  ├─► uninstall.rs                           purge Kitty's data at uninstall
  ├─► log_capture.rs, wizard.rs, util.rs
  │
state.rs        AppState (managed Tauri state): config, the engine handle,
                StackStatus, in-flight sessions, pending approvals, downloads.
```

**Local inference is LiteRT, linked into the engine** (`BigTinyV2/daemon/src/litert/`):
embeddings for memory on both platforms, and on Windows only the local
summarizer. There is no local chat, and no Ollama module: `provider_type:
"ollama"` survives only as a *remote* endpoint dialect.

**Hosting.** Desktop attaches to a shared `bigtiny2-daemon.exe`
(`lifecycle/bigtiny_v2.rs` over the `bigtiny2-client` crate): it reads the
handshake, proves the process is a V2 engine, and spawns one under a lock only
when none is running. Kitty registers as the app `kitty` and keeps its key in
the Credential Manager; a lost key is reclaimed with the handshake's
registration token, keeping Kitty's data. Nothing in Kitty kills the engine:
another app may be mid-turn. A settings change that only applies at start-up
asks the engine to restart itself (`POST /api/admin/restart`), which it does
only when no other app is attached or busy; otherwise the change waits and the
hub says who is in the way ("Restart anyway" forces it). Android links the same
engine and hosts it in-process (`bigtiny_embedded.rs`), because Android 10+
will not `exec()` a binary from app storage; there, start-up settings apply the
next time Kitty starts. Both sit behind the same HTTP boundary, so nothing
above `lifecycle/` knows which it has.

**The first app to start the engine decides its start-up settings**
(summarizer, token management, specialist limits). Settings → Advanced says
when that app was not Kitty. Anything that must differ per app lives in the
engine's per-app tables instead (e.g. `app_plugins`, which is how memory is
switched per app without a restart).

**Events.** Chat streams arrive per turn (`/api/chat/{id}/send`, 15 s
keepalives). Everything else Kitty needs to hear about, for any of its chats,
arrives on one app-scoped stream (`/api/apps/me/events`, `app_events.rs`):
approvals waiting (answered automatically when safe, otherwise put in front of
the user — inline if the chat is on screen, a blocking dialog otherwise, and the
overlay is summoned if no Kitty window is visible), scheduled runs, titles.

Dependency direction is top-to-bottom: `commands/` calls into
`lifecycle/`/`config/`/`bigtiny/`, never the reverse.

## Frontend (`src/`)

```
windows/{hub,overlay,screenshot-select}/App.tsx  one entry point per window label
  │   hub = chat + saved chats + settings + wizard, routed by `routeStore`
  │         (Android's whole UI, and desktop's full window; `overlay` and
  │          `screenshot-select` are desktop-only)
  │
  ├─► components/hub/        ChatWorkspace, HubBanners (engine restart waiting,
  │                          hotkey failure, V1 import), MobileDrawer (Android)
  ├─► components/chat/       Composer, MessageList/MessageItem, MessageActions,
  │                          ThinkingBox, ApprovalPrompt + ApprovalModal,
  │                          ToolCallCard, ProviderBadge + BranchToProvider
  │                          (handoff gate), ChatHeaderMenu — shared by
  │                          overlay and hub (rule 5)
  ├─► components/sessions/   SessionList (paged, searchable) — the sidebar on
  │                          desktop, the menu drawer on Android
  ├─► components/settings/   one panel per Settings section (Tool permissions
  │                          lists "always allow" rules)
  ├─► components/artifacts/  ArtifactsPane
  ├─► components/shared/     Dialog (focus trap, Escape/Back), ConfirmDialog
  │                          (confirmDialog(), typed confirmation), Banner,
  │                          ErrorDetail, StackStatusView
  ├─► components/wizard/     first run; repair opens at the broken step
  │
  ├─► stores/                zustand — render state only (rule 3)
  │     chatStore.ts (+ chat/ pure helpers), approvalStore.ts (every chat's
  │     waiting approvals), sessionStore.ts, stackStore.ts, routeStore.ts,
  │     adaptivePathwayStore.ts, mobileUiStore.ts
  │
  └─► lib/
        ipc.ts        the ONLY file that calls invoke() — typed wrappers
                      around every command, plus event listeners
        types.ts      TS mirrors of Rust structs (kept in sync by hand)
        escapeStack.ts / backDismiss.ts   Escape and Android Back close the
                      topmost layer first
        incoming.ts   shares and notification taps from Android
        handoff.ts, relativeTime.ts, pasteFiles.ts, platform.ts, ...
```

`lib/ipc.ts` is the chokepoint CLAUDE.md's "webview never fetches localhost
directly" rule depends on.

## Plugins (`plugins/`)

See `docs/PLUGINS.md` for the pattern. `kitty-tools`, `kitty-web` and
`kitty-wasm` (Rust) are MCP servers registered in the engine's own
`/api/mcp/servers` (`bigtiny::mcp::ensure_builtin_servers`), not spawned by
Kitty; on desktop they are bundled executables (`externalBin`), and on Android
they run in-process (`transport: "in_process"`), configured through an explicit
`InProcessConfig` rather than the process environment. `python plugins/build.py`
builds them and the engine, and `--verify-manifest` checks the committed
binaries match their source.

- **`kitty-tools`**: 26 tools on desktop, 24 on Android (no `lean_shell` /
  `lean_shell_ro`) — workspace, files, Word, Excel, PDF, scratchpad, cache,
  document handles, image reading — plus 4 visualization tools (table, SVG,
  chart, Mermaid) behind their own toggle.
- **`kitty-web`**: `lean_web_search`, `lean_web_search_read_chunk`,
  `lean_web_scrape`. DuckDuckGo and Bing together by default (language and
  country honoured on both); Brave when a key is configured.
- **`kitty-wasm`**: 4 tools running Python or any WASI module in a wasmtime
  sandbox.

The two memory engines — behavioural (`plugins/adaptive-pathway_rust`) and
factual (`plugins/memorabilia_rust`) — are linked into the engine and hosted per
app. They run only when the embedding model is on disk and the user has them
on; Kitty switches them per app (`PUT /api/apps/me/plugins/{plugin}`), which
takes effect at once. Adaptive Pathway learns from the dialogue; Memorabilia
learns only from the documents a turn brings in (pasted text, attached files,
scraped pages — `agent::memorabilia_harvest`). Each can be erased entirely from
Settings. Memorabilia and specialists are off on Android.

## Cross-cutting: who is the source of truth

1. **Conversations, schedules, providers as the engine uses them, MCP
   registrations, approval rules** → the engine. The frontend's `messages[]`
   is a reconstruction from `chat://*` events, never persisted app-side.
   Provider cards are Kitty's (`config.json` + secret store), and every one is
   synced into the engine.
2. **Settings** → `config.json`, written through `patch_config` (only the
   fields that changed), with `config://changed` telling every window.
3. **Secrets** → never `config.json`, never the webview. Desktop: Windows
   Credential Manager (`keyring`, service `kitty`); the engine seals the
   provider keys it holds with a key it keeps DPAPI-protected
   (`encryption.key.dpapi`). Android: AES-256-GCM under a non-exportable
   AndroidKeyStore key (`src/android/secrets.rs` over `SecretStore.kt`), which
   also holds the in-process engine's key.
