# AGENTS.md — Kitty

## Commands

|command|what|
|-|-|
|`pnpm tauri dev`|full-stack dev (vite on :1420 + Rust)|
|`pnpm build`|`tsc \&\& vite build`|
|`pnpm lint`|`eslint . \&\& prettier --check .`|
|`pnpm test`|`vitest run` (happy-dom)|
|`cargo clippy`|Rust lint|
|`cargo test`|Rust unit tests|
|`python plugins/build.py`|freeze the BigTiny daemon (`plugins/bigtiny/`) + Python plugins to `.exe` before release|

Lint before test, always.

## Workflow
1. Start in `/plan` mode — architect maps affected files and risks before any code is written.
2. Switch to `/build` mode — coder implements per the approved plan.
3. Invoke review agent — "Review these changes, find bugs, security issues, missed call sites."
4. Invoke verify agent — "Run build/lint/test, grep for all renamed exports."

## Rules
- Never scan directories — reference files explicitly with @path/to/file.
- Output unified diffs, never full files.
- Never leave a function unwired. Grep for all call sites after any change.
- Chain of Grounded Objectives: write numbered, testable steps before coding.

## Subagents
Located in .opencode/agents/:
- build.md — coder (read/edit/bash: allow)
- review.md — reviewer (read/grep/lsp: allow, edit/bash: deny)
- plan.md — architect (read/glob/grep: allow, edit/bash: deny)
- verify.md — checker (read/bash/grep: allow, edit: deny)

## Key gotchas

* **Backend is BigTiny (REST/SSE), not ACP.** Kitty spawns the BigTiny daemon (`src-tauri/src/lifecycle/bigtiny_proc.rs`) and talks to it over plain REST + one SSE streaming endpoint (`POST /api/chat/{id}/send`), auth'd via `X-API-Key`. All BigTiny client code lives in `src-tauri/src/bigtiny/` (`client.rs`, `sessions.rs`, `stream.rs`, `providers.rs`, `mcp.rs`). This module emits the same `chat://*`/`session://*` Tauri events regardless of backend, so the frontend is unaffected by backend internals.
* **`serde\_norway`** for YAML (recipe import/export), not the archived `serde\_yaml`. Don't add `serde\_yaml`.
* **Binary placeholders** in `src-tauri/binaries/` are empty files satisfying Tauri's build-time existence check. `python plugins/build.py` overwrites them before release. Packing with placeholders produces a broken app.
* **Types are hand-synced** between Rust structs and `src/lib/types.ts`. Look for sync comments like `// Mirrors Rust ...` and the header block listing file pairs.
* **Single IPC chokepoint**: `src/lib/ipc.ts` is the only file that calls `invoke()` or `listen()`. Everything else goes through it. Never import `@tauri-apps/api` directly in a component.
* **Config**: `%APPDATA%/Kitty/config.json`. Secrets go in Windows Credential Manager (`keyring`, service `kitty`), never in config.json or JS memory.
* **Multi-page Vite build**: four HTML entries (`overlay`, `main`, `settings`, `wizard` in `src/windows/<label>/`). Vite ignores `src-tauri/\*\*` during watch (locked dll/exe cause EBUSY).
* **Logging**: `tracing` with default filter `kitty\_lib=info,warn`. WARN/ERROR events are also captured to an in-memory ring buffer (`log\_capture.rs`) viewable in Settings → Advanced.
* **Release profile**: `panic = "abort"`, `opt-level = "s"`, `lto = true`, `strip = true` in Cargo.toml.

## Architecture

* `main.rs` calls `kitty\_lib::run()` (lib.rs). All commands registered in `generate\_handler!\[]`.
* `windows.rs` creates all four windows at setup (overlay hidden, frameless; others hidden until used).
* `state.rs` holds AppState: config, process handles (ManagedProcess), stack health, the BigTiny daemon handle, deep-link targets.
* Source-of-truth boundaries: (1) session state → BigTiny (render-only in frontend), (2) MCP server registrations → BigTiny's own `/api/mcp/servers` (see `bigtiny/mcp.rs`'s `ensure_builtin_servers`), (3) secrets → Windows Credential Manager.
* `commands/session/` has four files: mod.rs, crud.rs, prompt.rs, config.rs — all delegate to `bigtiny::sessions`/`bigtiny::stream`/`bigtiny::providers`.

## Testing

* Frontend: `vitest` with `happy-dom`. Test files per functional area: `chatStore.<feature>.test.ts` (e.g. `chatStore.toolloop.test.ts`).
* Rust: `cargo test` (unit tests only; no mock-server integration harness currently).
* Pipelines (`.github/workflows/plugins.yml`): plugin tests run `pytest -q` in each `plugins/<name>/` directory with pip-installed deps. BigTiny's own `pytest` suite lives in its separate repo.

## Frontend conventions

* **CSS**: plain CSS with custom properties only — no Tailwind, no CSS-in-JS. Theme contract: `--bg`, `--surface`, `--text`, `--accent`, `--border`, etc. in theme files under `src/themes/`.
* **Components**: function components only. Shared hooks in `src/hooks/`.
* **Stores**: Zustand. `chatStore.ts` delegates to extracted helpers in `src/stores/chat/\*.ts` (types.ts, messageUtils.ts, loopGuards.ts, errorUtils.ts, approvalUtils.ts, modeInfoCache.ts).
* **No `any`**: `@typescript-eslint/no-explicit-any: 'error'` in eslint config.
* **Prettier**: single quotes, es5 trailing commas, 100 print width.
