# Backlog

Deferred items. Per CLAUDE.md's Definition of Done, code carries no `TODO`s —
open an item here instead.

## Open

## Noted for later

- **Recipes (removed, not merely resolved).** This entry tracked the "recipes /
  skills" gap, then tracked the client-side template feature that filled it.
  Both are gone: recipes were **replaced by specialists** (delegate agents the
  model calls itself via `call_specialist`, defined daemon-side and bounded in
  wall-clock time and reasoning tokens — see CLAUDE.md). Every file this entry
  used to cite has been deleted: `src-tauri/src/config/{recipes,recipe_yaml}.rs`,
  `src-tauri/src/commands/recipes.rs`, `src/components/settings/Recipes.tsx`,
  `src/lib/recipes.ts`, and `chatStore.ts`'s `sendWithRecipe`. What users lost is
  a deterministic `/slug` shortcut; what they gained is delegation they never
  have to ask for. Kept as a tombstone because the trade is worth remembering,
  not as an open item.

- ChatML **import** path — Phase 11 export is one-way (`.chatml` + `.meta.json`).
  Re-import (reconstruct a session from the pair) is out of scope for v1.
- Per-turn provider/model in the export: we only track the session's current
  model in render state, so every turn is tagged with it. Capture true per-turn
  model if goosed later exposes it in `session/load` replay metadata.
- ~~Cross-platform support (Windows-only for v1 per project description §11).~~
  **Partly done:** Android shipped (`docs/ANDROID.md`, Phases 1–8). macOS and
  Linux are still out of scope — the overlay's Copilot-key hook, the screenshot
  capture path and the `keyring` store are all Win32.

- **Composer live markdown auto-formatting (shelved, code retained).** A
  contentEditable composer that converted `* `/`# `-`###### ` into live bullets
  and size-stepped headings as you typed, serializing back to markdown on send.
  Reverted to the plain `<textarea>` on 2026-07-19 by owner request ("disable
  the rich text editor for now, return to this later") — the feature worked but
  wasn't worth the contentEditable complexity yet. The DOM helpers and their
  unit tests are deliberately kept, unreferenced, at `src/lib/composerRichText.ts`
  and `src/lib/composerRichText.test.ts`; nothing imports them, so they cost
  nothing at runtime and aren't in any bundle. Two non-obvious findings are
  documented in their comments and worth re-reading before any retry:
  1. React's `onBeforeInput` prop is NOT the native `beforeinput` event — react-dom
     registers it as a synthetic polyfill over `compositionend`/`keypress`/`textInput`/`paste`,
     so `nativeEvent.inputType` is `undefined` and any `inputType === 'insertText'`
     check silently never fires. Attach a native listener with `addEventListener`.
  2. `Range.deleteContents()` empties a text node *in place* when both boundaries
     fall inside it, rather than removing the node — so `childNodes.length === 0`
     misses the "block is now empty" case; test `textContent === ''` instead.

- **The `pathway` MCP server can never connect on V2 — the model has lost
  `record`/`forget` on both platforms.** Found 2026-09-09 while verifying the
  Android migration on device; **pre-existing in V2, not caused by that work**.

  `MCPManager` holds `pathway: Option<Arc<PathwayEngine>>` and hands it to
  `builtin::connect`. `lib.rs:155` constructs the manager with `None`, and the
  comment there says why: *"No daemon-wide engine to hand over: the in-process
  `pathway` MCP server is now connected per app, holding that app's engine."*
  That per-app hand-over was never implemented — `connect_server` still passes
  `self.pathway.clone()`, which is `None` in the only non-test construction. So
  the arm refuses every time with *"pathway MCP server requested but the
  behavioral-memory engine is disabled"*, which reads like a config problem and
  is not one. Observed on device; the same warning will appear on desktop.

  What still works: recall, and the automatic turn-end/compaction learning
  passes, which run in-process on the agent loop and never go through MCP. What
  is lost is precisely the part that made memory correctable — the model can no
  longer drop a belief it has just been told is wrong. `bigtiny::mcp`'s own
  comment calls that out as the reason the arm exists at all.

  The fix has a clear shape: the row carries `app_id`, and `PluginHost` already
  opens an engine per app lazily, so `connect_server` can resolve the calling
  app's engine at connect time instead of reading a daemon-wide field. Then
  delete the `pathway` field rather than leaving a `None` that looks configurable.
  Worth a test in the shape of `every_advertised_builtin_actually_connects` but
  against a manager built the way `run()` builds it — the existing test passes
  because it hands `connect` an engine directly, which is exactly what production
  does not do.

- **In-process MCP servers never receive the `KITTY_ALLOWED_DIRS` grants
  (Android).** Found during the V2 migration (2026-09-09); **pre-existing, not a
  regression** — V1 had no grant mechanism at all, and `docs/VERSIONS.md`
  records Android as still using the narrow home-only behaviour.

  `mcp::manager::row_to_config` computes `scoped_env` for *every* transport, but
  the resulting map is only applied when spawning a child process. An in-process
  server is a linked library over a duplex pipe — `builtin::connect` takes no env
  map, by design — so it reads the *host* process environment, where neither
  `KITTY_ALLOWED_DIRS` nor `KITTY_ALLOWED_DIRS_FILE` is set. `kitty-tools`'
  `paths::allowed_roots` therefore falls back to home, which on Android is
  `KITTY_PLUGIN_HOME`. A file the user attached from outside that tree comes back
  `PATH_OUTSIDE_HOME` — the exact failure `mcp::kitty_grants` was written to fix
  on desktop. Note the daemon *does* write the grants file on Android
  (`agent::loop_` calls `publish` per turn); only the pointer to it is missing.

  Not fixed here because the obvious fixes are each wrong in a specific way, and
  choosing between them is a design decision rather than a mechanical one:

  * Setting the two vars in `bigtiny::mcp`'s `server_env` Android branch works,
    but that function's own doc says **"do not add new variables here"** — it
    mutates the process environment while the daemon's tasks are already running,
    and is only benign by luck for the three values already there.
  * Setting them in `bigtiny_env::daemon_env` is the sanctioned place (applied
    before the daemon exists), but Kitty would have to duplicate
    `kitty_grants::static_allowed_dirs`' definition of the static set, which is
    the daemon's to define and would drift.
  * Setting them daemon-side in `builtin::connect` has both the data (`data_dir`,
    the row's `app_id`) and the right timing, but reintroduces a `set_var` race
    against concurrently running tasks — `unsafe` in Rust 2024 for that reason.

  The likely right answer is to stop routing this through the environment for
  in-process servers at all: give `serve_in_process` an explicit grants argument,
  so the linked case is configured by call rather than by ambient state. That is
  a small API change across `kitty-tools`/`kitty-web`/`kitty-wasm` and
  `mcp::builtin`, and worth doing deliberately.

- **Ship the CPython WASI guest on Android (Phase 8 finding).** `kitty-wasm`
  is an in-process MCP builtin on Android, but its 26 MB `python-3.12.0.wasm`
  guest is not bundled: `app.path().resource_dir()` there is an asset URI, not
  a filesystem path, so `bigtiny::mcp`'s `is_file()` probe fails, the
  `KITTY_WASM_PYTHON` override is left unset, and `execute_math_python` /
  `wasm_python_run` fall back to downloading the guest on first use. A stale
  copy *was* being packaged (11 MB compressed of the AAB) and was removed —
  packaging it as an asset achieves nothing without an
  extract-to-app-storage-on-first-run step, since wasmtime needs a real path.
  Fix is that extraction step plus pointing the env var at it; until then the
  behaviour is a first-use download, which is graceful but not offline.

  **Update 2026-08-21:** that download now actually works. It could not before
  — `guest::data_dir()` resolved to an unwritable path on Android, so every
  write failed (see docs/ANDROID.md §2.4a). This item stays open: the download
  path being functional is not the same as being offline, and bundling still
  needs the extract-to-app-storage step described above.
