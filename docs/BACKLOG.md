# Backlog

Deferred items. Per CLAUDE.md's Definition of Done, code carries no `TODO`s —
open an item here instead.

## Open

- **Uninstall doesn't yet run the data cleanup (release step R1).**
  `kitty.exe --uninstall-cleanup` exists (`src-tauri/src/uninstall.rs`): it
  removes Kitty's data from the engine (`DELETE /api/apps/me?purge=true`, other
  apps untouched), Kitty's credentials, config, models, chat folders and the
  tool cache. `src-tauri/nsis-hooks.nsh` does not call it yet. It belongs in
  `NSIS_HOOK_PREUNINSTALL` when Tauri's "delete app data" checkbox is ticked,
  and it must run **before** `AskToStopDaemon`, because reaching the engine may
  start it.

## Noted for later

- **Scheduled tasks while Kitty is closed on Android (A5, decided against for
  v1).** On Android the engine lives in the app's process, so a schedule runs
  only while that process is alive; one that fell due while it was gone runs
  when Kitty next opens (the engine's start-up catch-up). The spike found a
  headless run feasible — WorkManager wakes the process, the Worker reads the
  engine's key from the SecretStore and starts the in-process engine through a
  JNI entry point, which catches up on due schedules and exits via
  `idle_exit_mins`. What makes it large is approvals: with no UI, nothing runs
  Kitty's safe-call policy (`approvals.rs`, today tied to an `AppHandle`), so
  every tool call would wait out the 10-minute timeout and be denied; answering
  from a notification also needs the app to attach to the already-running
  engine instead of starting a second one on the same database. Needs a device
  to build and test against.

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

- **ChatML import.** Export (`src-tauri/src/commands/export.rs`: `.chatml` +
  `.meta.json`, reasoning and the answering model per turn from the engine's
  stored history) is one-way. Re-importing a pair as a session is out of scope
  for v1.
- **macOS and Linux** are out of scope. What is Win32 today: the screenshot
  capture, the DPAPI protection of the engine's key, and the `keyring`
  backend feature.

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
