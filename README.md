# Kitty

A Windows-only desktop chat client: a hotkey-summoned floating overlay backed
by **BigTiny**, a chat-first REST/SSE agent daemon vendored in this repo, with
[Ollama](https://ollama.com) for local inference. Kitty is a **client only**
— all agent logic, tool execution, MCP handling, and model routing live in
BigTiny. Kitty owns window management, process lifecycle, configuration,
file/screenshot context, session history, an artifacts sidepane,
notifications, tool-approval UI, theming, and a first-run installer.

Full behavioral contract and architecture live in [`CLAUDE.md`](CLAUDE.md)
and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — this file is a shorter
map to get a checkout running.

## What it does

- **Overlay + full window**: a global hotkey slides a small always-on-top
  chat overlay in/out; "Expand" (or a second, dedicated hotkey) opens a full
  window with session history and an artifacts pane. Any number of full chat
  windows can be open at once, each on an independent conversation.
- **Two session modes**: a tool-free "thought partner" chat mode and an
  agentic mode with filesystem/shell tool access, directory-sandboxed to the
  session's own chat folder (plus an explicitly-set working directory in
  agentic mode) — anything outside that requires an explicit approval, never
  a silent allow or reject.
- **Local-first**: Ollama models run entirely on-device; remote/personal
  (e.g. Tailscale) providers are supported too, with a visible trust-tier
  badge and a context handoff gate when a session with history switches to a
  less-trusted provider.
- **Drag-and-drop file context, screenshot region capture, session
  history/search, an artifacts sidepane, tool-call approvals, notifications,
  a first-run setup wizard**, and a themeable UI (drop a custom CSS file in
  to restyle every window, no rebuild required).

## Repository layout

```
src/                  React + TypeScript frontend (Vite)
src-tauri/             Rust core (Tauri v2) — all I/O, window management,
                        process lifecycle, config, secrets
plugins/                Internal Python subsystems, frozen to standalone
                        .exe's and bundled via Tauri's externalBin
  bigtiny/              BigTiny itself — the chat backend daemon
  adaptive-pathway/     Tool-selection/response-style learning sidecar + MCP
  replacement-mcp/      Context-optimized shell/file/web/document MCP tools
  brave-mcp-search/     Brave Search MCP tool
  wasm-math-mcp/        Sandboxed math execution MCP tool
docs/                   Architecture, plugin, release, and backend docs
```

## Tech stack

- **Shell**: Tauri v2 (Rust core + web frontend), Windows-only target.
- **Frontend**: React 18 + TypeScript + Vite, plain CSS with custom
  properties for theming (no Tailwind/CSS-in-JS). State via Zustand.
- **Backend**: BigTiny (FastAPI/uvicorn, Python) — REST + one SSE streaming
  endpoint, session storage in SQLite, provider routing (Ollama/Anthropic/
  OpenAI-compatible), MCP tool-server management, HITL approval policy.
- **Rust crates of note**: `tauri-plugin-global-shortcut`,
  `tauri-plugin-notification`, `tauri-plugin-shell`, `tauri-plugin-dialog`,
  `tauri-plugin-single-instance`, `reqwest`, `tokio`, `keyring` (Windows
  Credential Manager — secrets never touch JS or plaintext disk), `windows`
  (Win32 APIs for the keyboard hook and screen capture), `sysinfo`.

## Getting started (dev)

Prerequisites: Node.js + [pnpm](https://pnpm.io), a Rust toolchain
(`rustup`), and Python 3.11+ for running BigTiny from source.

```bash
pnpm install

# One-time: install BigTiny's own dependencies into your Python environment
pip install -e plugins/bigtiny

pnpm tauri dev
```

In dev, Kitty launches BigTiny via `python -m bigtiny` against
`plugins/bigtiny/` rather than a bundled `.exe` — see
[`docs/bigtiny-backend.md`](docs/bigtiny-backend.md) for exactly how that's
configured and why it must be `python -m bigtiny` specifically (the Windows
Proactor event-loop factory stdio MCP servers need).

### Commands

| Command | What |
|---|---|
| `pnpm tauri dev` | Full-stack dev (Vite on :1420 + Rust) |
| `pnpm build` | `tsc && vite build` |
| `pnpm lint` | `eslint . && prettier --check .` |
| `pnpm test` | `vitest run` |
| `cargo clippy` (in `src-tauri/`) | Rust lint |
| `cargo test` (in `src-tauri/`) | Rust unit tests |
| `pytest` (in `plugins/bigtiny/`) | BigTiny's own test suite |
| `python plugins/build.py` | Freeze BigTiny + every internal plugin to `.exe` |

### Building a release

`src-tauri/binaries/` ships committed, zero-byte placeholder `.exe`s for
every plugin so a fresh clone can `cargo check`/`cargo build` at all (Tauri
validates every `externalBin` entry exists on disk, even for a plain build).
Before an actual release build, freeze the real binaries first:

```bash
python plugins/build.py
pnpm tauri build
```

See [`docs/RELEASE.md`](docs/RELEASE.md) for the full checklist and
[`docs/PLUGINS.md`](docs/PLUGINS.md) for how the freeze pipeline and the two
plugin-integration shapes (Kitty-managed process vs. BigTiny-managed MCP
server) work.

## Documentation map

- [`CLAUDE.md`](CLAUDE.md) — the authoritative spec: architectural rules,
  coding conventions, and the full original phased build plan.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — current, accurate module
  map (supersedes the phase-by-phase repository layout in `CLAUDE.md`).
- [`docs/bigtiny-backend.md`](docs/bigtiny-backend.md) — the BigTiny
  integration contract from Kitty's side.
- [`docs/PLUGINS.md`](docs/PLUGINS.md) / [`plugins/README.md`](plugins/README.md)
  — the internal-plugin freeze pipeline and how to add a new one.
- [`docs/RELEASE.md`](docs/RELEASE.md) — build/release checklist.
