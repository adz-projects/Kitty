# Kitty

An agentic AI chat client for **Windows and Android**, built on Tauri v2.

On Windows it is a hotkey-summoned floating overlay that expands into a full
window with session history and an artifacts pane. On Android it is a single
routed window with a bottom tab bar. Both run the same React component tree —
platform differences are a handful of `isAndroid()` gates and one CSS
breakpoint, never a forked UI.

Kitty is the **client layer**: window management, hotkeys, theming, tool
approval, file and screenshot context, provider configuration, and process
lifecycle. Everything an agent actually *does* — model routing, tool
execution, MCP, sessions, streaming — happens in **BigTiny**, a Rust REST/SSE
daemon in this repo at `plugins/bigtiny_rust/`.

## How it runs

The same daemon is hosted two different ways, behind one HTTP boundary:

| | Windows | Android |
|---|---|---|
| BigTiny | child process (`bigtiny-daemon.exe`) | linked in, hosted in-process |
| MCP tool servers | bundled `.exe` sidecars over stdio | in-process over `tokio::io::duplex` |

Android needs the in-process path because Android 10+ refuses to `exec()` a
binary out of app-writable storage. Nothing above `lifecycle/` knows the
difference — both sit behind the same localhost REST API, authenticated with a
per-launch secret that never reaches the webview.

## Tools

The agent gets its capabilities from three bundled MCP servers, all Rust, all
on by default and requiring no credentials.

**`kitty-tools` — 22 local-machine tools.** Shell execution; file
read/write/append/replace with pagination; workspace analysis; Word document
read/outline/**write** (including hyperlinks); Excel inspect/read; PDF
text/outline; a persistent scratchpad; and a content cache. Plus 3
WCAG-oriented visualization tools (accessible table, chart, Mermaid diagram)
behind their own Settings toggle.

**`kitty-web` — 3 tools.** Web scrape, plus web search and its paged
read-back. DuckDuckGo always works; Brave is preferred per-query when an API
key is configured (a separate, off-by-default toggle). Large result sets
offload to disk with a keyword index instead of flooding the context.

**`kitty-wasm` — 4 tools.** Runs Python — or any WASI module — inside a
wasmtime sandbox with enforced time and memory ceilings, no network, and no
filesystem beyond explicit mounts. Used for exact arithmetic, data filtering,
and statistics. The 26 MB CPython guest ships with the app on Windows, so
first use is offline; on Android it downloads once and is then cached
(bundling it there needs an extract-to-app-storage step — see
`docs/BACKLOG.md`).

**Adaptive Pathway** is not a server. The behavioral-memory engine
(`plugins/adaptive-pathway_rust/`) is statically linked into the daemon, so
recall is an in-process call on the agent loop rather than a network hop. It
extracts durable beliefs about you from conversation, decays and consolidates
them across sessions, and injects a diverse handful per turn — framed as
working assumptions to check a request against, never a profile to conform to.
Its `record`/`forget` tools are exposed to the model through the daemon's
in-process MCP registry, which is what lets the model drop a belief you tell it
is wrong. Browsable in Settings; per-session incognito from the chat header.

## Local inference

Kitty runs **no inference process of its own**, and there is no local chat —
chat always routes to a remote provider. LiteRT is linked into the daemon for
exactly two local jobs:

- **Semantic embeddings** for Adaptive Pathway's memory (EmbeddingGemma), on
  both platforms.
- **Compaction summarization**, on **Windows only**. Android hands that to the
  session's remote chat model instead, so no generative model runs on the
  phone — that was the fix for on-device GPU heat and artifacting.

`provider_type: "ollama"` survives only as a *remote* endpoint dialect for a
server you run yourself.

## Tech stack

- **Shell** — Tauri v2. Windows ships an NSIS installer; Android ships an AAB
  (`aarch64` is the only supported ABI).
- **Frontend** — React 18 + TypeScript + Vite. Zustand for UI state. Plain CSS
  with custom properties, so a theme is a single droppable `.css` file. No
  Tailwind, no CSS-in-JS.
- **Core** — Rust. All I/O lives here; the webview never fetches localhost
  directly, which keeps the daemon secret out of JS and avoids CORS entirely.
  Streaming reaches the UI as Tauri events.
- **Secrets** — Windows Credential Manager via `keyring`. On Android, AES-256-GCM
  sealed under a non-exportable AndroidKeyStore key (`keyring` has no Android
  backend — it silently degrades to an in-memory mock, so it is excluded from
  the Android dependency graph entirely).

## Getting started

Prerequisites: Node.js with [pnpm](https://pnpm.io), and a Rust toolchain via
`rustup`. **No Python and no Rust toolchain is needed by end users**, and none
is needed to run the app in dev either — BigTiny is pure Rust and runs straight
from source.

```bash
pnpm install
pnpm tauri dev
```

In dev, Kitty launches the daemon with `cargo run` against
`plugins/bigtiny_rust/`, so `pnpm tauri dev` works before `plugins/build.py`
has ever run.

### Commands

| Command | What |
|---|---|
| `pnpm tauri dev` | Full-stack dev (Vite on :1420 + Rust core) |
| `pnpm build` | `tsc && vite build` |
| `pnpm test` | `vitest run` |
| `pnpm lint` | `eslint . && prettier --check .` |
| `cargo test` (in `src-tauri/`) | Rust unit tests |
| `cargo clippy` (in `src-tauri/`) | Rust lint |
| `cargo test` (in `plugins/<name>/`) | A bundled plugin's own suite |
| `python plugins/build.py` | Build all four bundled binaries |

`plugins/build.py` is the one place Python is still involved, and only as a
script runner: every target it builds is Rust (`cargo build --release`). It
exists because it owns the target-triple naming convention Tauri's
`externalBin` expects.

### Building a release

`src-tauri/binaries/` holds committed placeholder `.exe`s so a fresh clone can
`cargo check` — Tauri validates that every `externalBin` entry exists on disk
even for a plain build. Those placeholders cannot run, so build the real ones
first:

```bash
python plugins/build.py
pnpm tauri build
```

Android, which needs an explicit target:

```bash
pnpm tauri android build --apk --target aarch64
```

`docs/RELEASE.md` has both lanes in full, including the LiteRT runtime files
the Windows daemon needs bundled alongside it.

## Documentation

| Doc | What |
|---|---|
| [`CLAUDE.md`](CLAUDE.md) | Architectural rules and coding conventions — the spec |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Current module map with dependency direction |
| [`docs/PLUGINS.md`](docs/PLUGINS.md) | How bundled plugins are built and integrated |
| [`docs/ANDROID.md`](docs/ANDROID.md) | The Android port: constraints, decisions, contracts |
| [`docs/RELEASE.md`](docs/RELEASE.md) | Build and release checklist, both platforms |
| [`docs/VERSIONS.md`](docs/VERSIONS.md) | Pinned versions and verified external contracts |
| [`docs/BACKLOG.md`](docs/BACKLOG.md) | Known gaps and deferred work |
| [`docs/bigtiny-backend.md`](docs/bigtiny-backend.md) | The daemon contract from Kitty's side |
| [`src/themes/README.md`](src/themes/README.md) | The theming contract for custom CSS |
