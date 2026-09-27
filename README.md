# Kitty

An agentic AI chat client for **Windows and Android**, built on Tauri v2.

On Windows it is a hotkey-summoned floating overlay that expands into a full
window with chat history and an artifacts pane. On Android it is a single
routed window with a menu drawer, and a share target other apps can send
text, images and documents to. Both run the same React component tree —
platform differences are a handful of `isAndroid()` gates and one CSS
breakpoint, never a forked UI.

Kitty is the **client layer**: windows, hotkeys, theming, tool approvals, file
and screenshot context, provider configuration, and attaching to the engine.
Everything an agent actually *does* — model routing, tool execution, MCP,
sessions, streaming, scheduling — happens in **BigTiny V2**, a Rust REST/SSE
engine in this repo at `BigTinyV2/daemon/`.

## How it runs

The same engine is hosted two different ways, behind one HTTP boundary:

| | Windows | Android |
|---|---|---|
| BigTiny V2 | a shared `bigtiny2-daemon.exe` Kitty attaches to (and starts if none is running) | linked in, hosted in-process |
| MCP tool servers | bundled `.exe`s over stdio | in-process over `tokio::io::duplex` |

On Windows the engine is shared: other apps can attach to the same instance,
each with its own chats, providers and schedules. Kitty registers as the app
`kitty` and authenticates with an app key kept in the Credential Manager —
never in the webview. It never kills the engine; when a start-up setting
changes, it asks the engine to restart, which happens only when no other app
is using it.

Android needs the in-process path because Android 10+ refuses to `exec()` a
binary out of app-writable storage. Nothing above `lifecycle/` knows the
difference.

## Tools

The agent gets its capabilities from three bundled MCP servers, all Rust, all
on by default and requiring no credentials.

**`kitty-tools` — 26 local-machine tools (24 on Android, which has no shell).**
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
filesystem beyond explicit mounts. The CPython guest ships with the app on
Windows; on Android it downloads once and is then cached.

### Approvals

The engine pauses on tool calls. Kitty answers the safe ones itself — file
operations inside the chat's own folders, shell commands that aren't
security-sensitive — and asks you about the rest: inline if that chat is on
screen, otherwise a dialog over whatever Kitty shows (summoning the overlay if
nothing is showing), plus a notification. "Always allow" covers the tool, or
for a shell call the command's first two words, and every such rule can be
revoked in Settings → Tool permissions.

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
- **Memorabilia** (`plugins/memorabilia_rust/`, desktop only) remembers what is
  true in the material you bring in. It learns from **documents, not
  dialogue** — text you pasted, files you attached, pages the model scraped —
  distils single factual claims from them, consolidates identical claims across
  sources, and weighs each by where it came from. Its `memorabilia_search` /
  `memorabilia_read_item` tools let the model look things up.

## Local inference

Kitty runs **no inference process of its own**, and there is no local chat —
chat always goes to a provider you connect. LiteRT is linked into the engine
for two local jobs:

- **Semantic embeddings** for memory (EmbeddingGemma), on both platforms.
- **Summarizing long chats**, on **Windows only** and optional: with the local
  summarizer model downloaded it happens on your computer; otherwise the chat's
  provider does it. Android always uses the provider, so no generative model
  runs on the phone.

`provider_type: "ollama"` survives only as a *remote* endpoint dialect for a
server you run yourself.

## Tech stack

- **Shell** — Tauri v2. Windows ships an NSIS installer; Android ships an AAB
  (`aarch64` is the only supported ABI).
- **Frontend** — React 18 + TypeScript + Vite. Zustand for UI state. Plain CSS
  with custom properties, so a theme is a single droppable `.css` file. No
  Tailwind, no CSS-in-JS.
- **Core** — Rust. All I/O lives here; the webview never fetches localhost
  directly, which keeps the app key out of JS and avoids CORS entirely.
  Streaming reaches the UI as Tauri events.
- **Secrets** — Windows Credential Manager via `keyring`; the engine keeps its
  own key DPAPI-protected. On Android, AES-256-GCM sealed under a
  non-exportable AndroidKeyStore key (`keyring` has no Android backend — it
  silently degrades to an in-memory mock, so it is excluded from the Android
  dependency graph entirely).

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
| `cargo ndk -t arm64-v8a clippy --lib` (in `src-tauri/`) | Android lint |
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

Android, which needs an explicit target:

```bash
pnpm tauri android build --aab --target aarch64
```

`docs/RELEASE.md` has both lanes in full.

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
| [`docs/bigtiny-backend.md`](docs/bigtiny-backend.md) | The engine contract from Kitty's side |
| [`BigTinyV2/API.md`](BigTinyV2/API.md) | The engine's routes |
| [`src/themes/README.md`](src/themes/README.md) | The theming contract for custom CSS |
