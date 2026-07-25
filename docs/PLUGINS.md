# Internal plugins

Kitty ships Python subsystems as **internal plugins**: independent, tested
Python packages, frozen to standalone Windows `.exe`s at build time and
bundled via Tauri's `externalBin` mechanism. End users need no Python, `uv`,
or `pip` — see `plugins/README.md` for the directory layout and how to add a
new plugin; this file covers the *pattern* in more depth. The BigTiny daemon
itself (Kitty's chat backend, vendored in-tree at `plugins/bigtiny/`) follows
the same freeze pipeline, even though it isn't a "plugin" in the
tool-augmentation sense — see `docs/bigtiny-backend.md`.

## Why frozen `.exe`s, not a bundled Python runtime

The alternative (embed CPython + pip-install deps at first run) was
considered and rejected: it still requires shipping and provisioning a full
Python environment on the user's machine, just relocated — it doesn't remove
the dependency, only hides where it lives. A frozen binary has zero runtime
dependency on anything Python-related.

## The two integration shapes, and why plugins are wired differently

| | `adaptive-pathway` (sidecar) | `replacement-mcp` / `adaptive-pathway-mcp` |
|---|---|---|
| What it is | HTTP sidecar (FastAPI/uvicorn) | stdio MCP server |
| Who spawns it | **Kitty** (`lifecycle/adaptive_pathway_proc.rs`) | **BigTiny** |
| Who monitors it | Kitty (health loop, `ManagedProcess`) | BigTiny (its own `/api/mcp/servers` status) |
| Registered via | Config field + Kitty's own process spawn | `bigtiny::mcp::ensure_builtin_servers` (upserts BigTiny's `/api/mcp/servers`) |
| Rust wiring | `lifecycle/adaptive_pathway_proc.rs`, `commands/adaptive_pathway.rs`, `adaptive_pathway/mod.rs` (HTTP client) | `bigtiny/mcp.rs`, `commands/mcp_servers.rs` — no lifecycle file, no `ManagedProcess` |

**This split is deliberate, not incidental.** A plugin that BigTiny itself
spawns (any stdio MCP server) should *never* also get a Kitty-side
`ManagedProcess`/health-probe — that would be two supervisors racing to own
one child process. Decide which category a new plugin falls into before
writing any Rust wiring:

- **Kitty-managed process** (HTTP sidecar, background daemon Kitty talks to
  directly): follow the Adaptive Pathway sidecar / BigTiny daemon pattern — a
  `lifecycle/<name>_proc.rs` with `spawn`/`ensure_running` + `probe_health`, a
  `ManagedProcess`/`DaemonHandle` field in `AppState`, commands for
  status/restart/enable.
- **BigTiny-managed MCP server** (stdio MCP server): follow the
  replacement-mcp / adaptive-pathway-mcp pattern — no lifecycle file, just an
  entry in `bigtiny::mcp::ensure_builtin_servers`'s upsert (registers/updates
  it in BigTiny's `/api/mcp/servers` by name) and a Settings toggle. BigTiny
  owns the process entirely.

## The freeze pipeline

```
plugins/<name>/               # bigtiny included — plugins/bigtiny/
  pyproject.toml         # pinned deps + a [project.scripts] entry point
  <name>.spec            # PyInstaller onefile spec (datas, hiddenimports)
  src/... or *.py
  tests/
```

`plugins/build.py`, for each target (`bigtiny`, `adaptive-pathway`,
`adaptive-pathway-mcp`, `replacement-mcp`, `wasm-math-mcp`, `brave-mcp-search`
— `python plugins/build.py` with no args builds all six):
1. `pip install -e ".[extras]"` — installs the target's own pinned
   dependencies (plus any optional-dependency-group extras the target needs,
   e.g. adaptive-pathway's `sidecar` vs `mcp` groups) into whatever Python
   environment is running the script.
2. `pyinstaller <name>.spec --noconfirm` — freezes the entry point named in
   `pyproject.toml`'s `[project.scripts]` to a onefile `.exe`.
3. Copies the result into `src-tauri/binaries/<exe-name>-x86_64-pc-windows-msvc.exe`
   — the exact filename Tauri's `bundle.externalBin` (in `tauri.conf.json`)
   expects.

**Tauri validates every `externalBin` entry exists on disk at build time —
even for a plain `cargo build`, not just packaging.** That's why
`src-tauri/binaries/` has *committed, empty placeholder* files for each
target: without them, a fresh clone can't even `cargo check` until someone
has run `plugins/build.py` once. See `src-tauri/binaries/README.md`. Before
an actual release build, always run `python plugins/build.py` to overwrite
the placeholders with real frozen executables — `tauri build` doesn't
distinguish a placeholder from a real binary, only that the file exists, so
packaging with placeholders in place produces an app whose plugins can't
start.

### Resolving the bundled path from Rust

Every plugin resolves its own bundled exe path the same way — next to the
currently-running app executable:

```rust
// config::bundled_plugin_path (src-tauri/src/config/mod.rs)
pub(crate) fn bundled_plugin_path(name: &str) -> Option<String> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = dir.join(name);
    candidate.exists().then(|| candidate.to_string_lossy().into_owned())
}
```

This returns `None` in dev (`cargo run`/`tauri dev` never copies the bundled
exe alongside the dev binary), in which case each plugin falls back to a bare
PATH-relative name — a developer working on the plugin itself can point the
relevant config field (`adaptive_pathway_launch_command`, `bigtiny_command`)
at `uv run ...`/`python -m ...` instead, entirely independent of this
resolution. `bigtiny::mcp::ensure_builtin_servers` uses the same resolution
for `replacement-mcp.exe`/`adaptive-pathway-mcp.exe`/`wasm-math-mcp.exe`/
`brave-mcp-search.exe` when registering them with BigTiny.

## Adding a new plugin

See `plugins/README.md`'s "Adding a plugin" section for the concrete
checklist. The one decision to make first: which of the two categories above
does it fall into?
