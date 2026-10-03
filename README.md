# Kitty

An agentic AI chat client for **Windows**, built on Tauri v2: a
hotkey-summoned floating overlay that expands into a full window with chat
history and an artifacts pane. The same code can also be compiled for Android
(see [Compiling for Android](#compiling-for-android)).

Kitty is the **client layer**: windows, hotkeys, theming, tool approvals, file
and screenshot context, provider configuration, and attaching to the engine.
Everything an agent actually *does* — model routing, tool execution, MCP,
sessions, streaming, scheduling — happens in **BigTiny V2**, a Rust REST/SSE
engine in this repo at `BigTinyV2/daemon/`.

## How it runs

Kitty attaches to a shared `bigtiny2-daemon.exe`, starting one if none is
running, and the engine runs the bundled MCP tool servers as `.exe`s over
stdio. The engine is shared: other apps can attach to the same instance, each
with its own chats, providers and schedules. Kitty registers as the app
`kitty` and authenticates with an app key kept in the Credential Manager —
never in the webview. It never kills the engine; when a start-up setting
changes, it asks the engine to restart, which happens only when no other app
is using it (or you choose "Restart anyway").

## Tools

The agent gets its capabilities from three bundled MCP servers, all Rust, all
on by default and requiring no credentials.

**`kitty-tools` — 26 local-machine tools.**
Shell execution; file read/write/append/replace with pagination; workspace
analysis; Word document read/outline/**write** (including hyperlinks); Excel
inspect/read; PDF text/outline; image reading; a persistent scratchpad; and a
content cache. Plus 4 visualization tools (accessible table, SVG diagram,
chart, Mermaid diagram) behind their own Settings toggle.

Long documents are extracted **once**, not once per page. Every paged reader
caches its full extraction keyed by the file's path, size and mtime, and hands
back a `document_id`; `lean_doc_read_chunk` and `lean_doc_search` then walk or
search the whole document from that cache without re-parsing it.

**`kitty-web` — 3 tools.** Web scrape (which also downloads a linked PDF,
Word, Excel or text file for the readers above), plus web search and its paged
read-back. DuckDuckGo and Bing are queried together, honouring the requested
language and country; Brave is preferred per query when an API key is
configured (a separate, off-by-default toggle). Large result sets offload to
disk with a keyword index instead of flooding the context.

**`kitty-wasm` — 4 tools.** Runs Python — or any WASI module — inside a
wasmtime sandbox with enforced time and memory ceilings, no network, and no
filesystem beyond explicit mounts. The CPython guest ships with the app.

### Approvals

The engine pauses on tool calls, and Kitty answers most of them itself. It
asks you only when a call reaches a file outside the chat's own folders or
runs a shell command that looks security-sensitive: inline if that chat is on
screen, otherwise in a dialog over whatever Kitty shows (summoning the overlay,
with a notification, if no Kitty window is showing). "Always allow" covers the
tool, or for a shell call the command's first two words, and every such rule
can be revoked in Settings → Tool permissions. An approval for a scheduled
task that nobody answers is denied after 10 minutes.

### Memory

Two memory engines are linked into the engine. Both need the optional
EmbeddingGemma model (downloaded in the wizard or Settings → Adaptive Pathway);
without it chat works exactly the same and memory is simply off. Each can be
paused per chat (incognito, in the chat's ⋯ menu) and erased entirely from
Settings.

- **Adaptive Pathway** (`plugins/adaptive-pathway_rust/`) learns how you work:
  durable beliefs extracted from conversation, decayed and consolidated across
  sessions, a diverse handful injected per turn — framed as working assumptions
  to check a request against, never a profile to conform to. Its
  `record`/`forget` tools let the model drop a belief you tell it is wrong.
- **Memorabilia** (`plugins/memorabilia_rust/`) remembers what is
  true in the material you bring in. It learns from **documents, not
  dialogue** — text you pasted, files you attached, pages the model scraped —
  distils single factual claims from them, consolidates identical claims across
  sources, and weighs each by where it came from. Its `memorabilia_search` /
  `memorabilia_read_item` tools let the model look things up.

## Local inference

Kitty runs **no inference process of its own**, and there is no local chat —
chat always goes to a provider you connect. LiteRT is linked into the engine
for two local jobs:

- **Semantic embeddings** for memory (EmbeddingGemma).
- **Summarizing long chats**, optionally: with the local summarizer model
  downloaded and chosen in Settings → Advanced, it happens on your computer;
  otherwise the chat's provider does it.

`provider_type: "ollama"` survives only as a *remote* endpoint dialect for a
server you run yourself.

## Tech stack

- **Shell** — Tauri v2, shipped as an NSIS installer.
- **Frontend** — React 18 + TypeScript + Vite. Zustand for UI state. Plain CSS
  with custom properties, so a theme is a single droppable `.css` file. No
  Tailwind, no CSS-in-JS.
- **Core** — Rust. All I/O lives here; the webview never fetches localhost
  directly, which keeps the app key out of JS and avoids CORS entirely.
  Streaming reaches the UI as Tauri events.
- **Secrets** — Windows Credential Manager via `keyring`; the engine keeps its
  own at-rest key DPAPI-protected.

## Installing

Download `Kitty_<version>_x64-setup.exe` from
[Releases](https://github.com/adz-projects/Kitty/releases) and run it. The
installer is **unsigned**, so SmartScreen may show "Windows protected your
PC": choose **More info → Run anyway**. Kitty installs for the current user
only.

Uninstall from Settings → Apps. Tick "Delete the application data" to also
remove Kitty's chats, settings, models, chat folders and saved keys (other
apps sharing the engine keep theirs).

## Getting started

Prerequisites: Node.js with [pnpm](https://pnpm.io), a Rust toolchain via
`rustup`, and [Git LFS](https://git-lfs.com) (the bundled binaries are LFS
objects). End users need no runtime of any kind.

```bash
git lfs install
pnpm install
pnpm tauri dev
```

If no built engine is bundled, dev runs it with `cargo run` against
`BigTinyV2/daemon/`, so `pnpm tauri dev` works before `plugins/build.py` has
ever run.

### Commands

| Command | What |
|---|---|
| `pnpm tauri dev` | Full-stack dev (Vite on :1420 + Rust core) |
| `pnpm build` | `tsc && vite build` |
| `pnpm test` | `vitest run` |
| `pnpm lint` | `eslint . && prettier --check .` |
| `cargo test` / `cargo clippy` (in `src-tauri/`) | Rust tests and lint |
| `cargo ndk -t arm64-v8a clippy --lib` (in `src-tauri/`) | Lint the Android build |
| `cargo test` (in `BigTinyV2/daemon/`, `plugins/<name>/`) | The engine's and a plugin's own suites |
| `python plugins/build.py [target]` | Build the bundled binaries and update `manifest.json` |
| `python plugins/build.py --verify-manifest` | Check the committed binaries match their source |

`plugins/build.py` is a script runner only: every target it builds is Rust
(`cargo build --release`). It owns the target-triple naming Tauri's
`externalBin` expects, stages the LiteRT runtime DLLs, and records a source
hash per binary in `src-tauri/binaries/manifest.json`.

### Building a release

The binaries in `src-tauri/binaries/` are committed through Git LFS; a clone
without LFS gets pointer files, which CI refuses. After changing the engine or
a plugin, rebuild it and commit the result:

```bash
python plugins/build.py
pnpm tauri build
```

`docs/RELEASE.md` has the full checklist.

### Compiling for Android

The project also compiles for Android (`aarch64` only). There the same engine
is linked into the app and hosted in-process, with the MCP servers in-process
too, because Android refuses to run a separate executable from app storage;
nothing above `src-tauri/src/lifecycle/` knows the difference.
`plugins/build.py` is not needed for it:

```bash
pnpm tauri android build --apk --target aarch64
```

`--target aarch64` is required. `docs/ANDROID.md` and `docs/RELEASE.md` have
the details.

## Documentation

| Doc | What |
|---|---|
| [`CLAUDE.md`](CLAUDE.md) | Architectural rules and coding conventions — the spec |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Current module map with dependency direction |
| [`docs/PLUGINS.md`](docs/PLUGINS.md) | How bundled plugins are built and integrated |
| [`docs/ANDROID.md`](docs/ANDROID.md) | Compiling for Android: constraints and decisions |
| [`docs/RELEASE.md`](docs/RELEASE.md) | Build and release checklist |
| [`docs/VERSIONS.md`](docs/VERSIONS.md) | Pinned versions and verified external contracts |
| [`docs/BACKLOG.md`](docs/BACKLOG.md) | Known gaps and deferred work |
| [`docs/bigtiny-backend.md`](docs/bigtiny-backend.md) | The engine contract from Kitty's side |
| [`BigTinyV2/API.md`](BigTinyV2/API.md) | The engine's routes |
| [`src/themes/README.md`](src/themes/README.md) | The theming contract for custom CSS |
