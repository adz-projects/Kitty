# Kitty internal plugins

This directory holds Python subsystems that ship as part of Kitty but are
maintained as independent, testable packages rather than living inside
`src-tauri/`. Each is frozen to a standalone Windows `.exe` at build time via
`build.py` and bundled through Tauri's `externalBin` mechanism (see
`src-tauri/tauri.conf.json`'s `bundle.externalBin`) — **end users never need
Python, `uv`, or `pip` installed**.

## Current plugins

| Plugin | Integration surface | Spawned by |
|---|---|---|
| `adaptive-pathway` | HTTP sidecar (FastAPI/uvicorn, default port 8700) | Kitty (`src-tauri/src/lifecycle/adaptive_pathway_proc.rs`) |
| `replacement-mcp` | stdio MCP server | goosed, registered via `src-tauri/src/goose_config.rs` |

Each plugin's integration surface is different by design — Kitty only ever
manages a process it *directly* spawns and monitors (the AP sidecar);
`replacement-mcp` is a goosed extension like any other, and Kitty's job is
limited to writing the registration entry into goose's `config.yaml`, not
supervising the child process itself.

## Directory shape

```
plugins/
  <plugin-name>/
    pyproject.toml       # pinned dependencies — the freeze's dependency closure
    src/... or *.py      # the plugin's own source
    tests/                # pytest suite for this plugin only
    docs/                 # plugin-specific docs (API contracts, etc.), if any
  build.py                 # freezes every plugin -> src-tauri/binaries/
  README.md                # this file
```

## Adding a third plugin

1. Create `plugins/<name>/` with its own `pyproject.toml` declaring a
   `[project.scripts]` console-script entry point (`main()` function) —
   `build.py` freezes that entry point with PyInstaller.
2. Add the plugin to `PLUGINS` in `build.py`.
3. Add the frozen binary's name (matching the console-script name) to
   `bundle.externalBin` in `src-tauri/tauri.conf.json`.
4. Wire it into Rust: either a new `lifecycle/<name>_proc.rs` (if Kitty
   spawns/monitors it directly, like Adaptive Pathway) or a
   `goose_config.rs` registration helper (if goosed spawns it, like
   replacement-mcp) — see `docs/PLUGINS.md` for the full pattern.
5. Add a pytest job for it in CI.

## Building locally

```
python plugins/build.py
```

Freezes every plugin and copies the resulting `.exe`s (with Tauri's
target-triple suffix) into `src-tauri/binaries/`. This is slow (PyInstaller
onefile builds take real time), so day-to-day development can instead point
a plugin's Rust-side launch command at `uv run <entry point>` — see each
plugin's own lifecycle wiring for the override.
