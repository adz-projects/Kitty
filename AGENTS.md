# AGENTS.md — Kitty

## Agent Execution Guidelines: Plan > Build > Test > Revise Loop

You are operating under a strict iterative execution framework. You must never jump straight into bulk coding without explicit verification and feedback cycles.

---

### Core Execution Loop

You must strictly follow this 4-step loop for all tasks beyond simple single-file edits or factual questions:
[1. PLAN] ➔ [2. BUILD] ➔ [3. TEST] ➔ [4. REVISE / RE-TEST]

---

#### Step 1: PLAN (Before Editing Code)
- **Analyze First:** Read the relevant codebase files, interfaces, and test files. Do not make assumptions about non-visible code.
- **Formulate Strategy:** Outline a concise, step-by-step implementation plan.
- **Define Verification:** State explicitly **how** you will verify your changes (e.g., specific unit test command, linter command, typecheck, or endpoint check).
- **Seek Confirmation:** For large refactors or complex features, pause and confirm the plan with the user before writing code.

---

#### Step 2: BUILD (Incremental Execution)
- **Small Commits/Edits:** Implement code changes in small, logical chunks rather than editing 10 files simultaneously.
- **Maintain Style:** Adhere strictly to existing code patterns, typing rules, and naming conventions in the repository.
- **No Mock Testing:** Do not delete, disable, or hardcode tests to make them pass artificially.

---

#### Step 3: TEST (Mandatory Execution)
- **Run Verification Commands:** Immediately execute the relevant build, type check, lint, and test commands in the terminal (e.g., `npm test`, `pytest`, `tsc --noEmit`, etc.).
- **Do Not Guess Success:** Never claim code works without seeing explicit green test/build output from tool execution.
- **Observe Output:** Capture stdout/stderr logs carefully to diagnose failures.

---

#### Step 4: REVISE (Systematic Bug Fixing)
- **Diagnose Before Fixing:** If tests or builds fail, do not randomly modify code. State the root cause based on error logs first.
- **Surgical Adjustments:** Make targeted fixes addressing the explicit error.
- **Re-test:** Re-run the tests. Repeat Steps 3 and 4 until all tests pass and typing/lint checks are clean.

---

### Hard Rules & Constraints

1. **Terminal Command Authority:** You must run terminal checks yourself. Do not ask the user to run `npm test` or `pytest` for you if tool access allows it.
2. **Context Preservation:** Keep terminal outputs clean. If a test command generates thousands of lines, pipe or filter it to show only relevant failures.
3. **Rollback Mindset:** If an approach fails after 2–3 revision attempts, halt, revert changes using Git (`git checkout` / `git restore`), and re-plan with an alternative approach.
4. **Completion Criterion:** A task is **ONLY** complete when code is written, tests/type-checks pass with 0 errors, and the implementation matches the original user goal.


## Commands

| Command | Where | What |
|---------|-------|------|
| `pnpm install` | root | Install frontend deps |
| `pnpm tauri dev` | root | Full dev loop (Vite :1420 + Rust hot-reload) |
| `pnpm build` | root | `tsc && vite build` |
| `pnpm lint` | root | `eslint . && prettier --check .` |
| `pnpm format` | root | `prettier --write .` |
| `pnpm test` | root | `vitest run` |
| `cargo clippy` | `src-tauri/` | Rust lint |
| `cargo test` | `src-tauri/` | Rust unit tests |
| `cargo test`, `cargo clippy` | `plugins/bigtiny_rust/` (and each Rust plugin dir) | Backend/plugin Rust tests |
| `python plugins/build.py` | root | Build the 4 bundled binaries to `.exe` (**desktop only**) |

### Android lane

Android shares the Rust core and the entire frontend; only the packaging and
the hosting shape differ. Full toolchain env and the trap list live in
`docs/RELEASE.md` § Android and `docs/ANDROID.md` §11 — the short version is
that `llama-cpp-sys-2` needs `cmake`, `ninja`, `libclang`, `CMAKE_GENERATOR=Ninja`,
a short `CARGO_TARGET_DIR`, and the NDK under `ANDROID_NDK`/`ANDROID_NDK_ROOT`
(*not* `ANDROID_NDK_HOME`, which it does not read).

| Command | Where | What |
|---------|-------|------|
| `cargo ndk -t arm64-v8a --platform 26 check --lib` | `src-tauri/` | The gating check. **`cargo check --target aarch64-linux-android` is not a substitute** — it silently skips `llama-cpp-sys-2`'s build script and passes on code that does not build. |
| `pnpm tauri android dev` | root | Dev loop on a connected device |
| `pnpm tauri android build` | root | Release AAB |
| `pnpm tauri android build --apk` | root | APK for sideloading |

**Do not run `plugins/build.py` for Android.** There are no Android sidecars
by design: `tauri.android.conf.json` clears `bundle.externalBin`, the daemon is
hosted in-process (`lifecycle/bigtiny_embedded.rs`), and the MCP servers
register with `transport: "in_process"`, because Android 10+ refuses to
`exec()` a binary in app-writable storage.

`src-tauri/gen/android/` **is tracked** (unlike `gen/schemas/`, which is
regenerated): it carries hand-made edits — the manifest's
`windowSoftInputMode`, `minSdk = 26`, the signing config. Its own `.gitignore`
excludes the build tree and the staged `.so`.

**Dev prerequisite**: none beyond the normal Rust/Node toolchains. BigTiny is now **pure Rust** (`plugins/bigtiny_rust/`). In dev Kitty runs it via `cargo run --manifest-path plugins/bigtiny_rust/Cargo.toml --bin bigtiny-daemon` (see `config::default_bigtiny_args`), so `pnpm tauri dev` works before `plugins/build.py` has ever run. The old Python-daemon flow and its `pip install -e plugins/bigtiny` prerequisite are gone.

**Release build order**: `python plugins/build.py` then `pnpm tauri build`. The freeze script overwrites placeholder `.exe`s in `src-tauri/binaries/` with real executables. Packaging with placeholders produces a non-functional app.

## Architecture at a glance

- **Tauri v2**, two targets: **Windows** (NSIS) and **Android** (AAB). Rust core (`src-tauri/`) + React 18/TS/Vite frontend (`src/`), shared by both.
- **3 window entry points**: `hub`, `overlay`, `screenshot-select`. Vite is a multipage build — see `vite.config.ts` rollup inputs (single `WINDOWS` array mirrors `windows.rs::url()`). Each has its own `index.html` under `src/windows/<label>/`. `hub` routes between chat / saved chats / settings / wizard in one window (`routeStore`); it is desktop's full window and Android's entire UI. `overlay` and `screenshot-select` are desktop-only.
- **Platform branching**: Rust uses `#[cfg(target_os = "android")]` / `#[cfg(desktop)]`; the frontend uses `isAndroid()` from `lib/platform.ts` and the `data-platform` attribute it stamps on the root for CSS. Prefer CSS at the mobile breakpoint over a JS branch where either works.
- **Backend**: BigTiny daemon. Pure Rust, source at `plugins/bigtiny_rust/`, frozen to `bigtiny-daemon.exe`. All chat/tool/MCP logic lives there — this app is the client layer. `plugins/bigtiny/` is the *retired* Python-original daemon, kept in-tree unbuilt as a behavioral oracle.
- **Config**: `%APPDATA%/Kitty/config.json` on Windows, the app-private data dir on Android (`config::app_base_dir`). **Secrets**: Windows Credential Manager via `keyring` (service `kitty`), never `config.json`, never JS. **On Android keyring falls through to an in-memory mock (D24)** — keys do not survive a relaunch; that is a known release blocker, not a design.
- **Plugin integration patterns** (critical distinction, see `docs/PLUGINS.md`):
  - *Kitty-managed process*: Kitty spawns, monitors via `ManagedProcess`/health loop. Pattern: `lifecycle/<name>_proc.rs`. Holds exactly one thing: the BigTiny daemon (`bigtiny_proc.rs`), and only on desktop — Android hosts the same daemon in-process (`bigtiny_embedded.rs`) behind the same HTTP boundary.
  - *BigTiny-managed MCP server* (stdio: `kitty-tools`, `kitty-web`, `kitty-wasm`): BigTiny spawns/owns. Kitty only upserts the registration via `bigtiny::mcp::ensure_builtin_servers`. No `ManagedProcess`.
  - *In-process MCP server* (`bigtiny_rust`, non-desktop hosts that can't `exec()`): the server links in as a library over an in-memory pipe — see `mcp::builtin` in `plugins/bigtiny_rust/` (`docs/PLUGINS.md`).
  - **Never mix** — two supervisors racing one child is a bug.

## Frontend rules

- `src/lib/ipc.ts` is the **only** file that calls `invoke()`. Every component goes through it — never call `invoke` directly from a component.
- **No `any`** — `@typescript-eslint/no-explicit-any` is an error.
- `@/*` path alias → `src/*` (configured in `tsconfig.json` + `vite.config.ts`).
- Zustand stores hold **render state only**. Session/conversation truth lives in BigTiny.
- Plain CSS with custom properties for theming. **No Tailwind, no CSS-in-JS.**
- Chat components (`components/chat/`) are shared verbatim between the `overlay` and `hub` windows, and between desktop and Android.

## Rust rules

- Every `#[tauri::command]` returns `Result<T, String>` with user-safe messages. Log details with `tracing`, don't surface internals.
- `thiserror` for error enums per module.
- The Rust plugins (`plugins/bigtiny_rust/`, `plugins/kitty-tools/`, `plugins/kitty-web/`, `plugins/kitty-wasm/`) are **standalone crates**, NOT workspace members of `src-tauri` (MSRV isolation, workspace-root-only `[profile.*]`, feature-unification reasons — see their `Cargo.toml` doc comments / `docs/PLUGINS.md`). `bigtiny_rust` depends on `kitty-tools` as a path dep purely for the in-process MCP server.

## Gotchas

- Tauri validates every `bundle.externalBin` entry exists on disk at **any** build time (even `cargo check`), resolved *by target triple*. Empty placeholder `.exe`s in `src-tauri/binaries/` are committed for this reason, and it is why Android needs `tauri.android.conf.json` to clear the list — otherwise the build demands `*-aarch64-linux-android` artifacts and fails in the build script, before any Rust compiles.
- The frontend never fetches `localhost` directly — all network calls go through the Rust side to keep secrets out of JS and avoid CORS.
- `state.rs` (`AppState`) is the single managed state object. Everything reads/writes through it.
- For module-level architecture, read `docs/ARCHITECTURE.md` (current, accurate). `CLAUDE.md` has the authoritative spec but its repository layout section is historical/aspirational.
