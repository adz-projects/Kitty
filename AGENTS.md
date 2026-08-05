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
| `pytest` | `plugins/bigtiny/` | BigTiny tests |
| `python plugins/build.py` | root | Freeze all 6 plugins to `.exe` |

**Dev prerequisite**: `pip install -e plugins/bigtiny` (one-time, installs BigTiny's Python deps). In dev, Kitty launches BigTiny via `python -m bigtiny` — the module entry point is required (installs the Windows Proactor event-loop factory stdio MCP servers need).

**Release build order**: `python plugins/build.py` then `pnpm tauri build`. The freeze script overwrites placeholder `.exe`s in `src-tauri/binaries/` with real executables. Packaging with placeholders produces a non-functional app.

## Architecture at a glance

- **Windows-only Tauri v2** app. Rust core (`src-tauri/`) + React 18/TS/Vite frontend (`src/`).
- **5 windows**: `overlay`, `main`, `settings`, `wizard`, `screenshot-select`. Vite is a multipage build — see `vite.config.ts` rollup inputs. Each window has its own `index.html` under `src/windows/<label>/`.
- **Backend**: BigTiny daemon (`plugins/bigtiny/`), vendored in-tree, frozen to `bigtiny-daemon.exe`. All chat/tool/MCP logic lives there. This app is the client layer.
- **Config**: `%APPDATA%/Kitty/config.json`. **Secrets**: Windows Credential Manager via `keyring` (service `kitty`), never `config.json`, never JS.
- **Plugin integration patterns** (critical distinction, see `docs/PLUGINS.md`):
  - *Kitty-managed process* (HTTP sidecars like BigTiny itself, adaptive-pathway): Kitty spawns, monitors via `ManagedProcess`/health loop. Pattern: `lifecycle/<name>_proc.rs`.
  - *BigTiny-managed MCP server* (stdio: `kitty-tools`, `kitty-web`, `kitty-wasm`, `adaptive-pathway-mcp`): BigTiny spawns/owns. Kitty only upserts the registration via `bigtiny::mcp::ensure_builtin_servers`. No `ManagedProcess`.
  - **Never mix** — two supervisors racing one child is a bug.

## Frontend rules

- `src/lib/ipc.ts` is the **only** file that calls `invoke()`. Every component goes through it — never call `invoke` directly from a component.
- **No `any`** — `@typescript-eslint/no-explicit-any` is an error.
- `@/*` path alias → `src/*` (configured in `tsconfig.json` + `vite.config.ts`).
- Zustand stores hold **render state only**. Session/conversation truth lives in BigTiny.
- Plain CSS with custom properties for theming. **No Tailwind, no CSS-in-JS.**
- Chat components (`components/chat/`) are shared verbatim between `overlay` and `main` windows.

## Rust rules

- Every `#[tauri::command]` returns `Result<T, String>` with user-safe messages. Log details with `tracing`, don't surface internals.
- `thiserror` for error enums per module.
- `kitty-tools` (`plugins/kitty-tools/`) is a **standalone Rust crate**, NOT a workspace member of `src-tauri` (MSRV isolation, profile config reasons — see its `Cargo.toml` doc comment).

## Gotchas

- Tauri validates every `bundle.externalBin` entry exists on disk at **any** build time (even `cargo check`). Empty placeholder `.exe`s in `src-tauri/binaries/` are committed for this reason.
- The frontend never fetches `localhost` directly — all network calls go through the Rust side to keep secrets out of JS and avoid CORS.
- `state.rs` (`AppState`) is the single managed state object. Everything reads/writes through it.
- For module-level architecture, read `docs/ARCHITECTURE.md` (current, accurate). `CLAUDE.md` has the authoritative spec but its repository layout section is historical/aspirational.
