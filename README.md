# Kitty

A chat client and agentic AI assistant for **Windows and Android**. On Windows
it includes a hotkey-summoned floating overlay; on Android it is a single
routed window with a bottom tab bar. Kitty ships with a useful library of
MCP-based tools for research, document parsing and creation, computation, and
system-level operations, and includes **Adaptive Pathways** — a knowledge-graph
system that learns your preferences and suggestion patterns while actively
resisting overfitting through ensemble diversity, novelty tracking, and
exploration incentives.

All model inference, session management, and tool orchestration runs in
**BigTiny**, a custom Rust REST/SSE daemon with llama.cpp linked in for local
inference. Kitty is the client layer: window management, hotkeys, theming, HITL
approval UI, file and screenshot context, provider configuration, and process
lifecycle. On Windows the daemon is a child process; on Android the same code
is hosted in-process, because Android will not execute a bundled binary out of
app storage.

## What it does

**Chat and agent modes.** A "thought partner" mode for open-ended
conversing with access to tools, and a tool-first agentic mode with filesystem,
shell, and MCP tool access. Chat actions are directory-sandboxed to the session's 
chat folder; anything outside it triggers a human-in-the-loop approval prompt.

**Tool library.** Bundled MCP server plugins give the agent access to:

* **kitty-tools** (Rust) — 18 always-on local-machine tools: shell execution,
file read/write/append with pagination, workspace analysis, Word document
read/outline/write, a persistent scratchpad, and a content cache, plus 4
WCAG 2.2 AA compliant visualization tools (accessible tables, SVG diagrams,
charts, and Mermaid diagrams rendered client-side from a bundled runtime),
gated by their own Settings toggle, and read-only Excel/PDF
tools. On by default, no credentials.
* **kitty-web** (Rust) — web article scraping and merged web search
(DuckDuckGo always available, Brave preferred per-query when an API key is
configured). On by default (Brave preference is a separate opt-in toggle).
* **kitty-wasm** (Rust) — run Python (or any WASI module) in a sandboxed
WebAssembly (wasmtime + WASI) interpreter for exact math, data filtering,
and statistical computation. On by default, no credentials required; its
CPython guest ships with the app so first use is offline.
* **adaptive-pathway** — the `record`/`forget` tools the model calls to
participate in the Adaptive Pathways learning loop. Not a separate server:
the engine is linked into the daemon and its tools are registered in-process,
so recall happens on the agent loop rather than over a socket.

**BigTiny backend.** A Rust daemon handling provider routing (Anthropic,
OpenAI-compatible endpoints, and self-hosted servers including Ollama),
local inference via llama.cpp, session persistence (SQLite),
SSE streaming with token-budget compaction, provider failover, MCP server
lifecycle, and a pattern-based HITL approval policy. Multiple sessions run
concurrently, each with its own provider, model, and persona configuration.

**Adaptive Pathways.** A knowledge-graph sidecar that builds a weighted graph
of learned preference edges across domains. It uses ensemble voting, inverse
propensity scoring, DPP-based diversity, Thompson sampling, and novelty
penalties to keep suggestions from collapsing into local optima. The system
surfaces ensemble "schisms" for user resolution, tracks exploration health
metrics, supports per-domain weight tuning, and lets the model itself flag
exploration nudges. Suggestions can be paused; learning continues at reduced
weight.

**Window chrome.** A global hotkey toggles a small always-on-top overlay; an
expand button opens a full window with session history, an artifacts pane,
and settings. Multiple full windows can be open simultaneously, each on an
independent session. Drag-and-drop file context and region screenshot capture
feed directly into the conversation.

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
  kitty-wasm/           Sandboxed WebAssembly compute MCP tool (Rust)
docs/                   Architecture, plugin, release, and backend docs
```

## Tech stack

* **Shell**: Tauri v2 (Rust core + web frontend), targeting Windows and Android.
* **Frontend**: React 18 + TypeScript + Vite, plain CSS with custom
properties for theming (no Tailwind/CSS-in-JS). State via Zustand. One
component tree across both platforms.
* **Backend**: BigTiny (Rust) — REST + one SSE streaming endpoint, session
storage in SQLite, provider routing (Anthropic/OpenAI-compatible/self-hosted),
in-process llama.cpp for local models, MCP tool-server management, HITL
approval policy.
* **Rust crates of note**: `tauri-plugin-global-shortcut`,
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

|Command|What|
|-|-|
|`pnpm tauri dev`|Full-stack dev (Vite on :1420 + Rust)|
|`pnpm build`|`tsc \&\& vite build`|
|`pnpm lint`|`eslint . \&\& prettier --check .`|
|`pnpm test`|`vitest run`|
|`cargo clippy` (in `src-tauri/`)|Rust lint|
|`cargo test` (in `src-tauri/`)|Rust unit tests|
|`pytest` (in `plugins/bigtiny/`)|BigTiny's own test suite|
|`python plugins/build.py`|Freeze BigTiny + every internal plugin to `.exe`|

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

* [`CLAUDE.md`](CLAUDE.md) — the authoritative spec: architectural rules,
coding conventions, and the full original phased build plan.
* [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — current, accurate module
map (supersedes the phase-by-phase repository layout in `CLAUDE.md`).
* [`docs/bigtiny-backend.md`](docs/bigtiny-backend.md) — the BigTiny
integration contract from Kitty's side.
* [`docs/PLUGINS.md`](docs/PLUGINS.md) / [`plugins/README.md`](plugins/README.md)
— the internal-plugin freeze pipeline and how to add a new one.
* [`docs/RELEASE.md`](docs/RELEASE.md) — build/release checklist.

