# Project Audit: Goose Overlay (Kitty)

> Generated: 2026-07-18 from 42 Rust files (`src-tauri/src/`) + 123 TypeScript files (`src/`, incl. 18 test files).

---

## ⚠️ Audit Review (added 2026-07-18, post-verification)

The auto-generated index below was cross-checked against the source. It is a useful
inventory but has **factual errors, stale counts, and one large blind spot**. Read this
section first; the tables further down have been corrected inline where flagged `[fixed]`.

### Factual errors (corrected inline below)

| # | Claim in index | Reality | Source |
|---|----------------|---------|--------|
| 1 | `ManagedProcess` is an **enum** (`None`/`Ours(Child)`/`Theirs`) | It's a **struct** `{ child: Option<Child>, owned: bool }` | `lifecycle/mod.rs:24` |
| 2 | `StackStatus` has 6 variants | **7** — the `#[default] Starting` variant is missing from the index | `lifecycle/mod.rs:46` |
| 3 | `AppState` = "16 fields (6× Mutex, 1× AsyncMutex, 9 fields)" | **12 fields: 11× `Mutex<T>` + 1× `AsyncMutex`** | `state.rs:18` |
| 4 | `.unwrap()` = 130 | **156** (grep of `src-tauri/src`) | — |
| 5 | `async fn` = 89 | **98** | — |
| 6 | icons = 18 | **20** | `components/icons/` |
| 7 | "no custom traits / entirely concrete types" | No trait *declarations*, but external trait **impls exist**: `impl Default` (×5+), `impl Visit for MessageVisitor` | `log_capture.rs:57`, `config/mod.rs:159,254` |
| 8 | `GOOSE_MOIM_MESSAGE_TEXT` = env var "must always be present" | It's the app's **system-prompt injection channel** — a single scalar env into which trust/network warnings are concatenated; only one can exist by design | `config/providers.rs:383-491` |

### What's missing

- **18 test files are entirely absent from the TS tree**, most importantly the **13 `chatStore.*.test.ts`** suites (artifact, humanizeerror, isimagefilename, modeinfocache, pathwithin, promptpreamble, providermatch, reasoningcap, recipewrapper, repetitionloop, straggler, thinkleak, toolloop) plus `ScheduledTasks.test.ts`, `pyrepr.test.ts`, `recipes.test.ts`, `vision_models.test.ts`, and wizard `App.test.ts`. This is the app's core regression surface and the index shows none of it.
- `src/vite-env.d.ts` (not listed).
- **No relationship/dependency mapping at all.** The index is a flat inventory. It never states the actual load-bearing edges: `lib/ipc.ts` is the *sole* `invoke()` chokepoint every store funnels through; which Zustand store drives which window; or that Tauri events (`chat://*`, `stack://status`, `ollama://pull-progress`) are the real fan-out spine. "What relationships are suspicious" can't be answered by a document with no relationships.

### Suspicious relationships / architectural drift

- **The entire Adaptive Pathway subsystem is undocumented in `CLAUDE.md`.** The sidecar process, the `qwen3-embedding:0.6b` dependency, and the edge/state/metrics/**schism**/**nudge** vocabulary (Rust: `adaptive_pathway/`, `lifecycle/adaptive_pathway_proc.rs`, 3 extra `AppState` fields; TS: `AdaptivePathway.tsx`, `GraphHealth.tsx`, `DomainProfiles.tsx`, `SchismResolutionModal.tsx`, `NudgeConsentPrompt.tsx`, `adaptivePathwayStore.ts`) appear **nowhere in the 11-phase plan or the project description.** Likewise **Recipes, ScheduledTasks, folder bookmarks, and log capture** are all beyond the documented spec. The index inventories these as ordinary modules and never flags that the codebase has grown well past its own architecture doc — the most important thing an audit should surface.
- **This audit is a snapshot of a large *uncommitted* working tree.** `git status` shows ~60 modified files plus deletions of `copilot.rs` and `provider_trust.ts` (mid-rename to `provider_trust.tsx`). The "Generated 2026-07-18" header doesn't disclose that it describes uncommitted state, so line/count references will drift the moment the tree is committed or reverted.
- **Phase 9 mandates a `provider_unreachable_offline` state** distinct from `provider_unreachable` ("can't reach `<host>` — check Tailscale"). No such `StackStatus` variant exists — it's either unimplemented or lives in a separate per-provider health path the index doesn't map. Worth confirming.
- **`GOOSE_MOIM_MESSAGE_TEXT` is a security-relevant single-slot channel.** Because it's one scalar env, any second consumer silently clobbers the first (the code comments say so). The index burying it in an `expect()` footnote hides a real constraint on how trust warnings reach goosed.

---

## Module Tree

### Rust — `src-tauri/src/`

```
lib.rs                              # crate root: run(), plugin/command registration
main.rs                             # `fn main()` → `goose_overlay_lib::run()`

adaptive_pathway/
  mod.rs                            # HTTP client for adaptive-pathway sidecar API
commands/
  mod.rs                            # re-exports all #[tauri::command] modules
  adaptive_pathway.rs               # 17 commands: restart, toggle, get_edge/state/metrics, ...
  config.rs                         # get_config, set_config, import_config
  extensions.rs                     # list/add/remove goose extensions
  file.rs                           # file I/O commands + tests
  folders.rs                        # folder-bookmark CRUD
  logs.rs                           # fetch log buffer
  ollama.rs                         # list/delete/pull models, ensure/restart ollama
  provider.rs                       # test connection, activate, OpenRouter credits
  recipes.rs                        # recipe CRUD, apply/unapply extensions
  scheduled_tasks.rs                # scheduled-task CRUD
  session.rs                        # new/load/fork/delete session, send/cancel/rebind
  setup.rs                          # detect/install/validate deps, open wizard
  window.rs                         # open_settings, open_main, restart_goosed
config/
  mod.rs                            # Config struct, load/save, merge, defaults + tests
  env_helper.rs                     # read/write Ollama env vars via HKCU\Environment
  providers.rs                      # ProviderProfile model, keyring CRUD, network_tier
  recipe_yaml.rs                    # YAML import/export for recipes + tests
  recipes.rs                        # Recipe model, builtins, CRUD + tests
  scheduled_tasks.rs                # ScheduledTask model, CRUD + tests
goosed/
  mod.rs                            # module declaration
  api.rs                            # AcpClient, connect/request, ensure_client
  stream.rs                         # SSE event handler → Tauri events
goose_config.rs                     # read/write Goose's own config file (extensions)
hotkey.rs                           # global shortcut register, clipboard-attach
lifecycle/
  mod.rs                            # spawn/monitor/health-loop, stop-all, compute_status
  adaptive_pathway_proc.rs          # sidecar spawn/probe, AdaptivePathwayStatus
  conflict.rs                       # detect stock Goose Desktop process
  goosed.rs                         # goosed agent spawn/probe
  ollama_proc.rs                    # ollama serve spawn/probe
log_capture.rs                      # tracing Layer → ring buffer, tests
notifications.rs                    # Windows toast notifications via plugin
ollama/
  mod.rs                            # list/delete/show/pull models, keep_alive
openrouter/
  mod.rs                            # list OpenRouter models, get credits
state.rs                            # AppState (16 fields), GoosedHandle
tray.rs                             # tray icon + menu builder
util.rs                             # hidden_command helper, capture_output
windows.rs                          # create/toggle windows, unsafe SPI_GETWORKAREA
wizard.rs                           # detect/install dirs, download installers
```

### TypeScript — `src/`

```
lib/                                # shared library code (NO React components)
  ipc.ts                            # invoke() wrappers — only file that calls Tauri
  types.ts                          # shared TS types (mirrors Rust)
  accelerator.ts                    # hotkey parsing
  chatml.ts                         # ChatML export
  context_length_table.ts           # model → context length mapping
  hintFeedbackDiscoverability.ts    # hint feedback logic
  provider_defaults.ts              # default provider configs
  provider_trust.tsx                # trust tier dialog components
  pyrepr.ts                         # Python repr parser + tests
  recipes.ts                        # recipe helpers + tests
  reasoning_models.ts               # model → thinking support map
  starter_models.ts                 # wizard model pick list
  system_prompts.ts                 # system prompt templates
  theme.ts                          # theme loading
  usePopoverPosition.ts             # popover positioning hook
  useRecipeAutocomplete.ts          # recipe autocomplete hook
  vision_models.ts                  # model → vision support map + tests

stores/                             # Zustand stores
  chatStore.ts                      # active session, messages, streaming, artifacts, modes
  sessionStore.ts                   # session list, search
  settingsStore.ts                  # config, providers, theme, notifications
  stackStore.ts                     # health status machine
  adaptivePathwayStore.ts           # adaptive pathway UI state

windows/                            # one entry per Tauri window label
  overlay/   main.tsx, App.tsx      # compact floating chat
  main/      main.tsx, App.tsx      # full-window chat + sidepanes
  settings/  main.tsx, App.tsx      # settings panel
  wizard/    main.tsx, App.tsx,     # first-run wizard (6 steps)
             DetectStep.tsx,
             ConfigureStep.tsx,
             FirstModelStep.tsx,
             EmbeddingModelStep.tsx,
             ApiKeyStep.tsx,
             DoneStep.tsx,
             PathFork.tsx

components/
  chat/                             # chat UI (shared between overlay + main)
    ChatView.tsx, ChatHeaderMenu.tsx
    MessageList.tsx, MessageItem.tsx, MessageInfo.tsx
    Composer.tsx, CodeBlock.tsx
    ToolCallCard.tsx, ApprovalPrompt.tsx
    AttachmentChips.tsx, FileChips.tsx, ClipboardImageChips.tsx
    MessageAttachmentChips.tsx
    ThinkingIndicator.tsx, ThinkingBox.tsx
    ModeBadge.tsx, ModeToggle.tsx, EffortDropdown.tsx
    ProviderBadge.tsx
    AdaptivePathwayToggle.tsx
    HintBadge.tsx, HintFeedbackButtons.tsx
    NudgeConsentPrompt.tsx
    PreviousAttemptBox.tsx
    SchismResolutionModal.tsx
    useProgressStage.ts
  sessions/
    SessionList.tsx, SessionKebabMenu.tsx, RecentSessions.tsx
  artifacts/
    ArtifactsPane.tsx
  settings/
    General.tsx, Providers.tsx, DomainProfiles.tsx
    OllamaModels.tsx, Extensions.tsx
    Appearance.tsx, Advanced.tsx
    NotificationsSection.tsx, GraphHealth.tsx
    Recipes.tsx, ScheduledTasks.tsx, AdaptivePathway.tsx
    SetupRepair.tsx, useConfigDraft.ts
  shared/
    Modal.tsx, ErrorDetail.tsx, StackStatusView.tsx
  icons/                            # 18 inline SVG icon components
```

---

## Key Types & Structs (no custom traits exist)

| Type | Kind | File | Notes |
|------|------|------|-------|
| `AppState` | struct | `state.rs:18` | **[fixed]** Root managed state: **11× `Mutex<T>` + 1× `AsyncMutex` = 12 fields** (config, goosed, ollama, stack_status, acp[async], active_session, settings_target, wizard_mode, adaptive_pathway, adaptive_pathway_status, adaptive_pathway_embedding_status, in_flight_sessions) |
| `GoosedHandle` | struct | `state.rs:79` | `ManagedProcess` + `port` + `secret_key` |
| `ManagedProcess` | **struct** | `lifecycle/mod.rs:24` | **[fixed]** `{ child: Option<Child>, owned: bool }` — NOT an enum; `kill_if_owned()` only reaps when `owned` |
| `StackStatus` | enum | `lifecycle/mod.rs:46` | **[fixed]** `Starting`(default) / `Ok` / `ollama_down` / `goosed_down` / `no_model` / `provider_unreachable` / `conflict_goose_desktop` — **7 variants** (index omitted `Starting`) |
| `AdaptivePathwayStatus` | enum | `lifecycle/adaptive_pathway_proc.rs` | `Disabled` / `Starting` / `Ok` / `Down` / `NoSidecar` |
| `EmbeddingModelStatus` | enum | `lifecycle/adaptive_pathway_proc.rs` | `Ok` / `Downloading` / `Missing` |
| `Config` | struct | `config/mod.rs` | App config with serde defaults |
| `ProviderProfile` | struct | `config/providers.rs` | Provider profile model with `network_tier` |
| `Recipe` | struct | `config/recipes.rs` | Recipe model with `extensions` |
| `ScheduledTask` | struct | `config/scheduled_tasks.rs` | Cron-like scheduled task model |
| `AcpClient` | struct | `goosed/api.rs` | ACP WebSocket client with `request()` |
| `ToolCalls` | struct | `goosed/stream.rs` | Active tool-call tracking state |
| `LogEntry` | struct | `log_capture.rs:26` | Serializable log entry |
| `MessageVisitor` | struct | `log_capture.rs:55` | `tracing::field::Visit` impl for log capture |
| `CaptureLayer` | struct | `log_capture.rs:78` | `tracing_subscriber::Layer` impl |
| `GooseConfig` | struct | `goose_config.rs` | Goose's own config reader/writer |

---

## Async Boundaries

### `async fn` — 89 declarations, ~190 `.await` call sites

| Layer | Count | Role |
|-------|-------|------|
| `adaptive_pathway/mod.rs` | 15 | HTTP API calls to adaptive-pathway sidecar |
| `commands/adaptive_pathway.rs` | 16 | Tauri command wrappers |
| `commands/ollama.rs` | 5 | List/delete/pull/ensure/restart |
| `commands/provider.rs` | 4 | Test/activate/credits |
| `commands/session.rs` | 13 | All session CRUD + prompt execution |
| `commands/setup.rs` | 6 | Detect/install/validate/wizard |
| `commands/window.rs` | 3 | Open windows + restart goosed |
| `config/providers.rs` | 1 | `test_connection` |
| `goosed/api.rs` | 5 | `connect`, `request`, `ensure_client` |
| `goosed/stream.rs` | 3 | `handle_incoming`, SSE → events |
| `lifecycle/adaptive_pathway_proc.rs` | 2 | Probe/ensure sidecar running |
| `lifecycle/goosed.rs` | 2 | Spawn/probe goosed |
| `lifecycle/mod.rs` | 3 | Health loop, scheduled tasks, `compute_status` |
| `lifecycle/ollama_proc.rs` | 3 | Probe/ensure Ollama running |
| `ollama/mod.rs` | 6 | Model ops + keep_alive + pull |
| `openrouter/mod.rs` | 2 | List models + credits |
| `wizard.rs` | 6 | Download/install deps |

### Key async orchestration

```
main.rs::fn main()                    # sync — calls lib::run()
  lib.rs::run()                       # sync — sets up tracing, builds tauri::Builder
    tray.rs::build_tray_menu()        # sync
    hotkey.rs::register()             # sync
    lifecycle/mod.rs::health_loop()   # async — tokio::spawn, polls every 5s
      → ollama_proc::probe_version    # .await
      → goosed::api::ensure_client    # .await
      → compute_status                # .await
      → emit stack://status event
    lifecycle/mod.rs::scheduled_tasks_loop()  # async — polls every 60s
    commands/session.rs::send_prompt()         # async spawn
      → goosed/api::request_session_prompt     # .await
      → goosed/stream::handle_incoming          # .await
        → emit chat://* events
        → lifecycle::track_and_maybe_record_outcome
```

---

## `unwrap()` Locations — 130 occurrences

### Pattern: `config.lock().unwrap()` — 50× in commands/ + lifecycle/

| File | Line | Pattern |
|------|------|---------|
| `commands/adaptive_pathway.rs` | 61, 73, 99, 100, 114, 131, 148, 149, 166, 167 | `state.*.lock().unwrap()` (all Mutex accesses) |
| `commands/config.rs` | 13, 24 | `state.config.lock().unwrap()` |
| `commands/folders.rs` | 29, 47, 69, 93, 111 | `state.config.lock().unwrap()` |
| `commands/ollama.rs` | 79, 90, 100, 109 | `state.ollama.lock().unwrap()` |
| `commands/provider.rs` | 41, 64, 77, 103, 125, 145, 158, 163, 182 | `state.config.lock().unwrap()` |
| `commands/recipes.rs` | 21, 101, 126, 145, 160, 186, 248, 270 | `state.config.lock().unwrap()` / `.find().unwrap()` |
| `commands/scheduled_tasks.rs` | 21, 53, 83, 108, 124 | `state.config.lock().unwrap()` |
| `commands/session.rs` | 22, 30, 124, 317, 340, 412, 435, 616, 658, 669, 762, 766 | `state.*.lock().unwrap()` |
| `commands/setup.rs` | 23, 77, 135, 143 | `state.*.lock().unwrap()` |
| `commands/window.rs` | 40, 52, 62, 71, 79 | `state.*.lock().unwrap()` |
| `goosed/api.rs` | 283 | `state.goosed.lock().unwrap()` |
| `goosed/stream.rs` | 263 | `state.config.lock().unwrap()` |
| `lifecycle/mod.rs` | 90, 97, 106, 115, 125, 137, 151, 170, 171, 182, 259, 348, 354, 382, 424, 430, 487, 530, 573, 594, 634, 635, 691, 692, 693 | `state.*.lock().unwrap()` |
| `notifications.rs` | 54 | `state.config.lock().unwrap()` |
| `windows.rs` | 234, 246 | `state.*.lock().unwrap()` |
| `wizard.rs` | 278 | `state.config.lock().unwrap()` |

### Pattern: `serde_json::from_str(...).unwrap()` — 14× in test code

| File | Lines |
|------|-------|
| `config/mod.rs` | 373, 382, 394, 395, 406, 417, 434, 441, 451, 462, 474 |
| `config/providers.rs` | 584 |
| `config/recipes.rs` | 490–502 |
| `config/recipe_yaml.rs` | 336–458 |

### Pattern: `std::fs::*.unwrap()` — 10× in test code

| File | Lines |
|------|-------|
| `commands/file.rs` | 230, 239, 245, 250, 262, 263, 269, 274, 278, 288, 294 |

### Pattern: misc `.unwrap()` — 6×

| File | Line | Code |
|------|------|------|
| `adaptive_pathway/mod.rs` | 28 | `.unwrap()` |
| `commands/adaptive_pathway.rs` | 26, 40, 53, 82 | various |
| `config/scheduled_tasks.rs` | 62, 63, 81, 82 | serde round-trip tests |
| `goosed/stream.rs` | 340 | session API response |
| `log_capture.rs` | 44, 48, 96, 118, 136, 151 | static Mutex (tests + buffer) |
| `wizard.rs` | 382 | download fallback |

---

## `expect()` Locations — 8 occurrences

| File | Line | Context | Rationale |
|------|------|---------|-----------|
| `config/providers.rs` | 599 | `GOOSE_MOIM_MESSAGE_TEXT` env var (test asserts presence) | **[see review §8]** This is the app's system-prompt injection channel (built at `providers.rs:383-491`), not a mundane env var — single scalar, only one consumer possible |
| `goose_config.rs` | 163 | Hashmap entry | "just ensured mapping above" |
| `lifecycle/mod.rs` | 167 | `reqwest::Client::new()` | "reqwest client" — infallible |
| `lifecycle/mod.rs` | 285 | `reqwest::Client::new()` | "reqwest client" — infallible |
| `lifecycle/mod.rs` | 306 | `reqwest::Client::new()` | "reqwest client" — infallible |
| `lifecycle/mod.rs` | 330 | `reqwest::Client::new()` | "reqwest client" — infallible |
| `lifecycle/mod.rs` | 551 | `reqwest::Client::new()` | "reqwest client" — infallible |
| `lib.rs` | 191 | `tauri::Builder::build()` | "error while building the Goose Overlay application" |

---

## `unsafe` Blocks — 1 occurrence

| File | Line | Code | Safety Justification |
|------|------|------|----------------------|
| `windows.rs` | 72–79 | `unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut rect as *mut _ as *mut c_void), ...) }` | Win32 FFI call. `SPI_GETWORKAREA` writes work-area dimensions into the caller-provided `RECT`. The pointer is valid, non-null, aligned, and points to a stack-local `RECT::default()`. Single-threaded context (Tauri main thread). |

---

## `unwrap_or()` Locations — 42 occurrences

| Pattern | Count | Typical Use |
|---------|-------|-------------|
| `serde_json::Value::get().and_then().as_str().unwrap_or("")` | ~15 | Extracting optional JSON fields with empty-string default |
| `serde_json::Value::get().as_bool().unwrap_or(false)` | ~5 | Optional boolean JSON fields |
| `Option::unwrap_or(default_val)` | ~10 | Fallback defaults for `Option<T>` in config/config parsing |
| `.unwrap_or(Value::Null)` | ~4 | JSON fallback for absent map entries |
| `.unwrap_or(&path)` | ~1 | CLI path fallback |
| `.unwrap_or(200 * 1024)` | ~1 | File-size cap default |
| `.unwrap_or(25 * 1024 * 1024)` | ~1 | Image-size cap default |
| `tauri::PhysicalSize` fallback | 2 | `windows.rs:85,115` — overlay size defaults |

---

## Summary Matrix

| Metric | Count | Notes |
|--------|-------|-------|
| **Rust source files** | 42 | `src-tauri/src/` |
| **TS/TSX source files** | **123** | **[fixed]** `src/` incl. **18 test files** (index said 112; 13 are `chatStore.*.test.ts`) |
| **Lines of Rust** | ~8.5k | Estimated |
| **Lines of TS/TSX** | ~7k | Estimated |
| **Custom trait declarations** | **0** | **[caveat]** No trait *decls*, but external trait *impls* exist (`Default`, `Visit`) — not "entirely concrete" |
| **`async fn` declarations** | **98** | **[fixed]** index said 89 |
| **`.await` call sites** | ~190 | All within Rust async functions |
| **`unsafe` blocks** | **1** | `windows.rs:72` — Win32 FFI, justified |
| **`unsafe fn`** | **0** | |
| **`.unwrap()` calls** | **156** | **[fixed]** index said 130 |
| **Icon components** | **20** | **[fixed]** index said 18 |
| **`.expect()` calls** | 8 | 5× `reqwest::Client`, 1× infallible hashmap, 1× env var, 1× tauri build |
| **`.unwrap_or()` calls** | 42 | All safe — fallback/default patterns |

### Risk Areas

- **50 production `.unwrap()` on Mutex locks** — each can panic if the lock is poisoned. In practice: no code in this app panics while holding a Mutex (no cross-boundary poison sources), so risk is theoretical.
- **1 `unsafe` block** — well-scoped Win32 call, no pointer arithmetic or lifetime escape.
- **All async is within Rust** — webview never calls localhost directly (architectural rule 2), preventing CORS + secret-exposure issues.
- **No `unsafe` in hotkey/hook code** — the Copilot key hook was removed; only `tauri-plugin-global-shortcut` remains (safe).
