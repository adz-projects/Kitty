# Plan: outputs folder, artifacts drawer, output cards

Status: **built** (see `docs/VERSIONS.md`, "Outputs folder, artifacts drawer, output cards"). This was the brief written for a coding subagent. It
can be handed to one as-is.

## Rules for whoever implements this

- Read `CLAUDE.md` first and follow its conventions: plain CSS, `base.css`
  contains no colour values (colours come from theme custom properties), no
  new dependencies, TS strict with no `any`, every `#[tauri::command]` returns
  `Result<T, String>`, in-app dialogs rather than `window.confirm`.
- The working tree has many **uncommitted** changes from earlier work (window
  chrome, chat styling, artifacts pane restyle). They are intended. Build on
  top of them. Do not revert, stash, reset, checkout or commit anything.
- A dev build may be running with hot reload; that is fine.
- On this Windows machine, Bash commands longer than about 8,000 characters
  are silently truncated. Write file content with the Write/Edit tools.

## Current state

- `src/components/hub/ChatWorkspace.tsx` renders the hub: chat list
  (`SessionList`), centre (`ChatView`), and `ArtifactsPane`
  (`src/components/artifacts/ArtifactsPane.tsx`). A title-bar button
  (MenuIcon) toggles the pane.
- A "grow the window when the pane opens" experiment was rejected, and its
  removal is **half done, so the frontend does not compile**:
  - Already gone: `src/lib/windowGrow.ts`, its test, and the
    `core:window:allow-set-size` capability line.
  - Still present: in `ChatWorkspace.tsx`, the `resizeWindowBy` import and the
    `paneMode` / `centerLock` / `centerRef` / `PANE_WIDTH` logic and inline
    `gridTemplateColumns` style; in `ArtifactsPane`, the `drawer` prop and
    `artifacts-drawer` class; in `src/themes/base.css`, the
    `.main-window > .artifacts-pane.artifacts-drawer` rules, an
    `overflow: hidden` on the desktop `.main-window`, and a
    `--chat-list-width` variable (the variable may stay).
- Artifacts today (`src/stores/chatStore.ts`,
  `src/stores/chat/messageUtils.ts`, `src/stores/chat/types.ts`):
  `artifacts: Artifact[]` is filled from tool calls (`deriveArtifact` +
  `isArtifactInScope`), user attachments (`source: 'user'`), and a disk poll
  `refreshArtifactsFromDisk` that lists the session's working directory `cwd`
  (`ipc.listDirectory`, returning
  `FileEntry { name, path, size, modified, is_dir }`). `subfolders` holds the
  directories found there. `pruneMissingArtifacts` drops vanished files. The
  poll lives in `ArtifactsPane.tsx`.
- The model is told to use relative paths inside the conversation's working
  directory, in `DEFAULT_SYSTEM_PROMPT`
  (`src-tauri/src/config/providers/mod.rs`), applied through
  `system_prompt_for`. That is used at session creation
  (`src-tauri/src/commands/session/crud.rs`), in `commands/provider.rs`, and
  for scheduled tasks (`commands/scheduled_tasks.rs`, which also uses
  `DEFAULT_SYSTEM_PROMPT` directly).
- The persisted config field `show_artifacts` (read and written in
  `ChatWorkspace.tsx`) currently decides whether the pane is open.
- Chat folders are created in `src-tauri/src/commands/session/mod.rs`
  (`fresh_chat_folder`, `resolve_cwd`).
- Turn lifecycle in `chatStore.ts`: a live turn starts in `send` (where
  `busy: true` is set) and ends in the `onComplete` handler (see also
  `onChatError`). Tool calls arrive in the handler that calls
  `deriveArtifact`. `replaying` is true while a resumed session's history is
  replayed.

## What to build

### 1. Finish removing the grow-the-window experiment

Remove everything listed as "still present" above. On desktop the pane is
**always** a sliding drawer over the right edge of the window: absolute,
`right: 0`, full height, `border-left`, `box-shadow: var(--overlay-shadow)`,
with the existing `artifacts-in` slide animation under
`prefers-reduced-motion: no-preference`. It never takes a grid column and
never changes the chat's layout: nothing in the conversation, its header row
or the composer may move when it opens or closes. Keep
`position: relative` on the desktop `.main-window`; drop the
`overflow: hidden`. Android's bottom sheet (`.artifacts-sheet`) keeps working
as it does now.

### 2. The drawer never opens on its own

- A small Zustand store (for example `src/stores/artifactsStore.ts`):
  `open` (starts `false` on every launch), `unseen`, `setOpen`, `toggle`,
  `markNew` (sets `unseen` only while the drawer is closed), and a reset for
  when the window switches chats. Opening the drawer clears `unseen`.
- `ChatWorkspace` uses this store for both the desktop drawer and the Android
  sheet. The frontend stops reading and writing `show_artifacts` entirely; the
  Rust config field stays so old config files still parse.
- The title-bar artifacts button is always visible on desktop. While `unseen`
  is true its icon is tinted red with the theme's `--danger` colour, and its
  `title` / `aria-label` says there is something new. It returns to normal
  once the drawer has been opened. Keep the small count beside the icon while
  the drawer is shut and there are outputs.
- The drawer closes with the title-bar button, a ✕ in its header, and Escape
  (register with `src/lib/escapeStack.ts` the way other dismissable layers
  do).
- `unseen` is cleared when the user loads a different chat or starts a new
  one.

### 3. The pane shows only the chat's `outputs` folder

- Outputs live in `<cwd>/outputs`, where `cwd` is the session's working
  directory. One shared frontend module (for example `src/lib/outputs.ts`)
  holds the directory name, `outputsDir(cwd)`, and a function mapping any path
  to its top-level entry under the outputs directory (or null if outside),
  handling `\` and `/` and case-insensitive comparison on Windows.
- Rust: when a chat folder is created, also create its `outputs` subdirectory,
  best effort (log a warning on failure; never fail session creation). The
  frontend tolerates the directory not existing.
- Tell the model. In `src-tauri/src/config/providers/mod.rs`, add a constant
  instruction appended (after a blank line) to **whichever** prompt applies,
  default or custom, so every new chat gets it, and never twice:

  > Files the user asked you to produce — a document, a report, a dataset, a
  > folder of downloads — go in the `outputs/` folder inside the working
  > directory (create it if it does not exist). Working files you make along
  > the way — scripts, scratch notes, caches, intermediate data — stay outside
  > it. Only put something in `outputs/` if the user asked for it.

  Cover every path that sets a session's system prompt, including the ones
  that use `DEFAULT_SYSTEM_PROMPT` directly. Update the Rust unit tests. Check
  `BigTinyV2/prompt_determinism.md`; do not change the engine.
- The pane lists exactly the top-level entries of the outputs directory, files
  **and** folders (a folder of 50 PDFs is one entry). The listing comes from
  disk, polled while the drawer is open, and refreshed when the chat's `cwd`
  changes and when a turn ends. Attachments and anything outside `outputs` no
  longer appear. The listing may live in the new store, tagged with the `cwd`
  it belongs to so a stale listing is never shown.
- Decide what happens to the now-unshown `artifacts` / `subfolders` state in
  `chatStore` (it is also carried in the window handoff, replay and tests):
  keep it internally consistent, or remove it cleanly. Not half.
- Pane header: "Artifacts" plus count. "Open folder" opens the outputs
  directory (falling back to `cwd`). Empty state: "Files you ask Kitty to make
  will appear here." Keep the current row design.

### 4. Output cards at the end of the turn that made them

- A turn that creates or changes something in `outputs` ends with one card per
  new top-level output, rendered after the answer text and before the message
  actions (`src/components/chat/MessageItem.tsx`).
- **Live turn:** remember the outputs listing when the turn starts, list again
  when it completes or errors, and attach every new or modified top-level
  entry (compare `modified` and `size`) to the turn's last assistant message
  (for example `outputs?: OutputRef[]`). If anything was attached, call
  `markNew()`. Guard against the user having switched chats meanwhile.
- **Resume / replay:** rebuild cards from replayed tool calls whose derived
  path lies inside the outputs directory, on the assistant message that holds
  the call. Never call `markNew()` during replay. One card per entry per
  message. Once the current listing is loaded, hide a card whose entry no
  longer exists, and take size and file-or-folder from the listing.
  - Accepted limitation: an output created only by a shell command gets its
    card live but not after a resume. Nothing is persisted app-side.
- **Card content:** file or folder icon, name, a one-line description derived
  locally (a file-type label such as "Word document", "PDF", "Python script",
  "Folder", plus size when known), and an action.
  - Hub: a link-style "Show in artifacts" that opens the drawer.
  - Overlay window (no artifacts pane): "Show in folder".
  - Clicking the name opens the file and reports failure inline.
  - Android: Download, as `ArtifactCard` does there.
- **Styling:** quiet outlined cards in the UI font, muted description,
  wrapping row, each at most about 22rem wide. No colour literals.

### 5. Tests and docs

- Vitest: the store, the outputs path helper, the new-or-modified diff, the
  file-type label helper, and any existing tests the change breaks.
- Docs: `docs/ARCHITECTURE.md` where artifacts are described, a short section
  in `docs/VERSIONS.md`, and the artifacts sentence in `CLAUDE.md`'s "Known
  risks" if it becomes wrong.

## Verification

Run until clean: `pnpm exec tsc --noEmit`, `pnpm exec eslint src`,
`pnpm exec prettier --check` on touched files (about 32 unrelated files
already fail), `pnpm test`, `pnpm build`; in `src-tauri`, `cargo fmt --check`
on touched files, `cargo clippy --all-targets`, and `cargo test --lib` for the
changed modules.

## Decisions made in the plan (open to change)

- `outputs` lives inside the chat's working folder. A chat pointed at a
  project folder gets an `outputs` folder there, and files edited in place in
  the project are not shown as artifacts.
- The instruction is added when a chat is created, so existing chats will not
  know about `outputs/`.
- Attached files no longer appear in the pane.
- Card descriptions come from file type and size, not from the model.
- The red tint is in memory only and does not survive a restart.
