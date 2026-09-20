# Backlog

Deferred items. Per CLAUDE.md's Definition of Done, code carries no `TODO`s —
open an item here instead.

## Open

- **`SpecialistRun` carries no host, so Settings cannot say what answered.**
  `src-tauri/src/bigtiny/specialists.rs`'s `SpecialistRun` has no
  provider/model field, so Settings -> "Recent delegate runs" cannot honour its
  own copy ("which specialist answered which request, **and on what**"). The
  daemon does know: `Orchestrator::host_actually_used` resolves it and
  `chat://subagent-status` already carries it (0.10.8). This is the execution
  *row* not persisting it.

- **Three specialist fields are silently dropped on save.** Rust's `Specialist`
  and `SpecialistSpec` (`src-tauri/src/bigtiny/specialists.rs`) are missing
  `reasoning_cap_tokens`, `reasoning_cap_fraction` and `fan_out`, all of which
  `src/lib/types.ts` and `src/components/settings/Specialists.tsx` read and
  write. Serde drops them on the way through, so those form controls are dead:
  a user sets a reasoning cap or a fan-out mode, the UI shows it saved, and the
  daemon never sees it. Fixing it is adding the fields; the reason it is worth
  an entry is that the failure is invisible from the UI.

## Noted for later

- **Heavier document extraction (OCR / tables) for memorabilia ingest.** The
  turn-end memorabilia harvest (`agent::memorabilia_harvest`) extracts attached
  files and scraped documents via `kitty_tools::extract`, which is
  `pdf-extract` + `lopdf` for PDFs, `calamine` for spreadsheets, `quick-xml`
  for docx. Baseline is strong on **text** (~99–100% word recall on digital
  PDFs) but has **no table-structure reconstruction and no OCR** — scanned /
  image-only PDFs come back empty (flagged, not read). Deliberately not adding
  `docling` (or any OCR engine) now: it is Python + ML models, which breaks
  Kitty's Rust-only, no-Python-runtime, no-bundled-models stance (CLAUDE.md) and
  would bloat the Windows and Android bundles substantially for a marginal gain
  on the text a fact-memory actually needs. Revisit as an **optional** path (a
  docling sidecar, or a Rust OCR/table extractor) if scanned-PDF or table
  fidelity turns out to matter — the extraction seam is one function
  (`extract_document_text`), so swapping/augmenting it is localized.

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

- **Compaction model swap: MiniCPM5-2B-LiteRT considered, declined (2026-09-19).**
  Evaluated replacing the Windows summarizer model `gemma-4-E2B-it.litertlm`
  (repo `litert-community/gemma-4-E2B-it-litert-lm`, `curated_models.ts` /
  `DEFAULT_SUMMARIZER_GGUF`) with `mlboydaisuke/MiniCPM5-2B-LiteRT`. Declined:
  it is a **hybrid-reasoning** model that emits a `<think>` block before every
  answer unless given `ThinkingConfig(enable_thinking=false)` (which the
  summarizer loader does not set, and which needs `litert-lm-rust` ≥ 0.16) — a
  poor fit for a deterministic JSON-summarization slot, and its int4 variant is
  documented to frequently not close its reasoning within a 3584-token budget.
  Size is not a win either: int8 ≈ 2.6 GB vs Gemma's 2.59 GB (int4 is 1.55 GB
  but is the problematic variant). The only upside is its Apache-2.0/ungated
  license vs Gemma's gate, which is not worth the behavioral risk for a slot
  that already works. If revisited: swap the `curated_models.ts` entry + a
  `LEGACY_SUMMARIZER_TAGS`-style migration for `DEFAULT_SUMMARIZER_GGUF`, and
  add `ThinkingConfig(enable_thinking=false)` to the summarizer loader.

- **`lean_read_image` on a non-vision provider (item 5 follow-up, 2026-09-19).**
  The new `lean_read_image` tool is advertised to every session regardless of
  the active provider's vision capability, because the daemon tracks no
  vision/accepts-images flag per provider (user-turn images are gated only by
  Kitty's frontend). If a non-vision model calls the tool, the loop injects an
  `image_url` user turn that a strict non-vision provider will 400 on. The tool
  description says it is only useful with a vision model, but the real fix is to
  relay the Kitty provider's `accepts_images` into the daemon and gate either
  the tool advertisement or the image injection on it.
