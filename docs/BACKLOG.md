# Backlog

Deferred items. Per CLAUDE.md's Definition of Done, code carries no `TODO`s —
open an item here instead.

## Open

## Noted for later

- **Recipes (resolved).** The item below this note used to track the "recipes /
  skills" gap left by the Round-2 `goose run --recipe`-can't-be-launched-via-ACP
  finding. Shipped instead as client-side-interpreted templates — a recipe's
  `instructions`/`prompt`/`extensions` attach to an ordinary chat turn (session-
  scoped extension add + a hidden `<recipe>` preamble on the outgoing prompt,
  `chatStore.ts`'s `sendWithRecipe`) rather than shelling out to the real CLI
  runner. Trade-off, by design: the real runner's `response`-schema enforcement,
  `retry`, and `sub_recipes` subagent delegation aren't reachable this way and
  are out of scope. Management/editor: `src/components/settings/Recipes.tsx`;
  invocation: `/slug free text` in the composer (`src/components/chat/Composer.tsx`,
  `src/lib/recipes.ts`); data model + YAML import/export:
  `src-tauri/src/config/{recipes,recipe_yaml}.rs`.

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
