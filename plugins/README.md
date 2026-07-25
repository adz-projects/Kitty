# Kitty internal plugins

This directory holds Python subsystems that ship as part of Kitty but are
maintained as independent, testable packages rather than living inside
`src-tauri/`. Each is frozen to a standalone Windows `.exe` at build time via
`build.py` and bundled through Tauri's `externalBin` mechanism (see
`src-tauri/tauri.conf.json`'s `bundle.externalBin`) — **end users never need
Python, `uv`, or `pip` installed**. The BigTiny daemon (Kitty's chat backend,
vendored in-tree at `plugins/bigtiny/`) follows the exact same freeze
pipeline — see `docs/bigtiny-backend.md` and `docs/PLUGINS.md`.

## Current plugins

| Plugin | Integration surface | Spawned by |
|---|---|---|
| `bigtiny` | HTTP+SSE daemon (Kitty's chat backend) | Kitty (`src-tauri/src/lifecycle/bigtiny_proc.rs`) |
| `adaptive-pathway` | HTTP sidecar (FastAPI/uvicorn, default port 8700) | Kitty (`src-tauri/src/lifecycle/adaptive_pathway_proc.rs`) |
| `adaptive-pathway-mcp` | stdio MCP server (`decide`/`record_outcome`/...) | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` |
| `replacement-mcp` | stdio MCP server | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` |
| `wasm-math-mcp` | stdio MCP server (sandboxed Python/NumPy execution) | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` — on by default |
| `brave-mcp-search` | stdio MCP server (Brave Search LLM Context API) | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` — off by default, needs an API key (Windows Credential Manager, not config.json) |

Each plugin's integration surface is different by design — Kitty only ever
manages a process it *directly* spawns and monitors (the AP sidecar, and the
BigTiny daemon itself); the two stdio MCP servers are BigTiny extensions like
any other, and Kitty's job is limited to keeping their registration entries
(command path, enabled state) in BigTiny's `/api/mcp/servers` up to date, not
supervising the child process itself.

## Directory shape

```
plugins/
  <plugin-name>/
    pyproject.toml       # pinned dependencies — the freeze's dependency closure
    src/... or *.py      # the plugin's own source
    tests/                # pytest suite for this plugin only
    docs/                 # plugin-specific docs (API contracts, etc.), if any
  build.py                 # freezes every target -> src-tauri/binaries/
  README.md                # this file
```

## Adding a new plugin

1. Create `plugins/<name>/` with its own `pyproject.toml` declaring a
   `[project.scripts]` console-script entry point (`main()` function) —
   `build.py` freezes that entry point with PyInstaller.
2. Add the plugin to `PLUGINS` in `build.py` (dir, spec file, exe name, any
   optional-dependency extras it needs installed).
3. Add the frozen binary's name (matching the console-script name) to
   `bundle.externalBin` in `src-tauri/tauri.conf.json`.
4. Wire it into Rust: either a new `lifecycle/<name>_proc.rs` (if Kitty
   spawns/monitors it directly, like Adaptive Pathway's sidecar) or an entry
   in `bigtiny::mcp::ensure_builtin_servers`'s upsert (if it's a stdio MCP
   server BigTiny should spawn, like replacement-mcp) — see `docs/PLUGINS.md`
   for the full pattern.
5. Add a pytest job for it in CI.

## Building locally

```
python plugins/build.py
```

Freezes every target and copies the resulting `.exe`s (with Tauri's
target-triple suffix) into `src-tauri/binaries/`. This is slow (PyInstaller
onefile builds take real time), so day-to-day development can instead point
a plugin's Rust-side launch command at `uv run <entry point>` — see each
plugin's own lifecycle wiring for the override.
