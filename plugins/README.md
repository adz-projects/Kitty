# Kitty internal plugins

This directory holds subsystems that ship as part of Kitty but are
maintained as independent, testable packages rather than living inside
`src-tauri/`. Most are Python, frozen to a standalone Windows `.exe` at build
time via PyInstaller; one (`kitty-tools`) is Rust, built with a plain `cargo
build --release`. Either way, `build.py` bundles the result through Tauri's
`externalBin` mechanism (see `src-tauri/tauri.conf.json`'s
`bundle.externalBin`) — **end users never need Python, `uv`, `pip`, or a Rust
toolchain installed**. The BigTiny daemon (Kitty's chat backend, vendored
in-tree at `plugins/bigtiny/`) follows the same Python freeze pipeline — see
`docs/bigtiny-backend.md` and `docs/PLUGINS.md`.

## Current plugins

| Plugin | Language | Integration surface | Spawned by |
|---|---|---|---|
| `bigtiny` | **Rust** (`plugins/bigtiny_rust/`) | HTTP+SSE daemon (Kitty's chat backend) | Kitty (`src-tauri/src/lifecycle/bigtiny_proc.rs`) |
| `adaptive-pathway` | Python | HTTP sidecar (FastAPI/uvicorn, default port 8700) | Kitty (`src-tauri/src/lifecycle/adaptive_pathway_proc.rs`) |
| `adaptive-pathway-mcp` | Python | stdio MCP server (`decide`/`record_outcome`/...) | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` |
| `kitty-tools` | **Rust** | stdio MCP server, 21 tools: shell/workspace/5 file/3 word/4 cache/4 scratchpad (always on) + 4 visualization tools: table/SVG diagram/chart/Mermaid (gated by its own env var) — no network calls | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` — on by default |
| `kitty-docs-web` | Python | stdio MCP server, 8 tools: 2 PDF, web scrape, 3 Excel, lean_web_search + lean_web_search_read_chunk. **Retired** — its web tools moved to `kitty-web` and its PDF/Excel tools to `kitty-tools` (lopdf/calamine). Source kept in-tree as the behavioral oracle | — |
| `kitty-web` | **Rust** | stdio MCP server, 3 tools: lean_web_search + lean_web_search_read_chunk (count-tiered: ≤5 normal Brave-with-DuckDuckGo-fallback, 6-10 queries both engines, >10 offloads to disk with a keyword index) + lean_web_scrape. Same tool names and response envelopes as the `kitty-docs-web` originals | BigTiny; stdio on desktop, in-process via `bigtiny_rust::mcp::builtin` where exec() isn't available |
| `kitty-wasm` | **Rust** | stdio MCP server, 4 tools: execute_math_python / wasm_python_run (Python in a wasmtime+WASI sandbox), wasm_run_module (any WASI module), wasm_guest_status. No network, no filesystem beyond explicit mounts, runtime-enforced time/memory ceilings. Replaces the retired `wasm-math-mcp`; its CPython guest ships with the app (offline first use) | BigTiny; stdio on desktop, in-process via `bigtiny_rust::mcp::builtin` |
| `wasm-math-mcp` | Python | **Retired** — superseded by `kitty-wasm`. Despite the name it was neither WebAssembly nor math tools: one tool (`execute_math_python`) running Python in a `multiprocessing` worker behind an AST allowlist (networkx, no NumPy). Source kept in-tree as the behavioral oracle | — |

`replacement-mcp`, `brave-mcp-search`, `visualizations`, `kitty-docs-web`,
and `wasm-math-mcp` are **retired** — all of their tools now live inside
`kitty-tools`/`kitty-web`/`kitty-wasm` above.
Their source stays in this directory, unbuilt (absent from `build.py`'s
`PLUGINS` dict). `replacement-mcp`/`brave-mcp-search`/`kitty-docs-web`/
`wasm-math-mcp`'s source remains the oracle to re-verify those ports against
if a behavioral gap ever surfaces;
`visualizations`' Rust rebuild deliberately diverges instead of porting it
(three of its four diagram types were static clipart) — see
`docs/PLUGINS.md`.

Each plugin's integration surface is different by design — Kitty only ever
manages a process it *directly* spawns and monitors (the AP sidecar, and the
BigTiny daemon itself); the stdio MCP servers are BigTiny extensions like any
other, and Kitty's job is limited to keeping their registration entries
(command path, enabled state) in BigTiny's `/api/mcp/servers` up to date, not
supervising the child process itself.

## Directory shape

```
plugins/
  <plugin-name>/
    pyproject.toml       # pinned dependencies — the freeze's dependency closure (Python)
    Cargo.toml            # standalone crate, own [[bin]] (Rust — kitty-tools only)
    src/... or *.py       # the plugin's own source
    tests/                # test suite for this plugin only
    docs/                 # plugin-specific docs (API contracts, etc.), if any
  build.py                 # freezes every target -> src-tauri/binaries/
  README.md                # this file
```

## Adding a new plugin

1. **Python**: create `plugins/<name>/` with its own `pyproject.toml`
   declaring a `[project.scripts]` console-script entry point (`main()`
   function) — `build.py` freezes that entry point with PyInstaller.
   **Rust**: create `plugins/<name>/` as a standalone crate (own
   `Cargo.toml`, not a workspace member of `src-tauri` — see
   `plugins/kitty-tools/Cargo.toml`'s doc comment for why) with a `[[bin]]`
   matching the exe name.
2. Add the plugin to `PLUGINS` in `build.py` (dir, exe name, plus `spec`/
   `extras` for Python or `kind: "rust"` for Rust — see the dict's own doc
   comment).
3. Add the frozen binary's name (matching the console-script/`[[bin]]` name)
   to `bundle.externalBin` in `src-tauri/tauri.conf.json`.
4. Wire it into Rust: either a new `lifecycle/<name>_proc.rs` (if Kitty
   spawns/monitors it directly, like Adaptive Pathway's sidecar) or an entry
   in `bigtiny::mcp::ensure_builtin_servers`'s upsert (if it's a stdio MCP
   server BigTiny should spawn, like kitty-tools) — see `docs/PLUGINS.md`
   for the full pattern.
5. Add a test job for it in CI.

## Building locally

```
python plugins/build.py
```

Freezes every target and copies the resulting `.exe`s (with Tauri's
target-triple suffix) into `src-tauri/binaries/`. This is slow (PyInstaller
onefile builds take real time), so day-to-day development can instead point
a plugin's Rust-side launch command at `uv run <entry point>` — see each
plugin's own lifecycle wiring for the override.
