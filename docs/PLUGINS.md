# Internal plugins

Kitty ships two Python subsystems as **internal plugins**: independent,
tested Python packages under `plugins/`, frozen to standalone Windows `.exe`s
at build time and bundled via Tauri's `externalBin` mechanism. End users need
no Python, `uv`, or `pip` — see `plugins/README.md` for the directory layout
and how to add a third plugin; this file covers the *pattern* in more depth.

## Why frozen `.exe`s, not a bundled Python runtime

The alternative (embed CPython + pip-install deps at first run) was
considered and rejected: it still requires shipping and provisioning a full
Python environment on the user's machine, just relocated — it doesn't remove
the dependency, only hides where it lives. A frozen binary has zero runtime
dependency on anything Python-related.

## The two current plugins, and why they're wired differently

| | `adaptive-pathway` | `replacement-mcp` |
|---|---|---|
| What it is | HTTP sidecar (FastAPI/uvicorn) | stdio MCP server |
| Who spawns it | **Kitty** (`lifecycle/adaptive_pathway_proc.rs`) | **goosed** |
| Who monitors it | Kitty (health loop, `ManagedProcess`) | goosed (its own `extensions/list`) |
| Registered via | Config field + Kitty's own process spawn | `goose_config::ensure_extension_registered` (writes goose's `config.yaml`) |
| Rust wiring | `lifecycle/adaptive_pathway_proc.rs`, `commands/adaptive_pathway.rs`, `adaptive_pathway/mod.rs` (HTTP client) | `commands/replacement_mcp.rs` only — no lifecycle file, no `ManagedProcess` |

**This split is deliberate, not incidental.** A plugin that goosed itself
spawns (any stdio MCP extension) should *never* also get a Kitty-side
`ManagedProcess`/health-probe — that would be two supervisors racing to own
one child process. Decide which category a new plugin falls into before
writing any Rust wiring:

- **Kitty-managed process** (HTTP sidecar, background daemon Kitty talks to
  directly): follow the Adaptive Pathway pattern — a `lifecycle/<name>_proc.rs`
  with `ensure_running`/`probe_health`, a `ManagedProcess` field in
  `AppState`, commands for status/restart/enable.
- **goosed-managed extension** (stdio MCP server): follow the replacement-mcp
  pattern — no lifecycle file, just a `goose_config::ensure_extension_registered`
  call at startup and a Settings toggle. goosed owns the process entirely.

## The freeze pipeline

```
plugins/<name>/
  pyproject.toml         # pinned deps + a [project.scripts] entry point
  <name>.spec            # PyInstaller onefile spec (datas, hiddenimports)
  src/... or *.py
  tests/
```

`plugins/build.py`, for each plugin:
1. `pip install -e .` — installs the plugin's own pinned dependencies into
   whatever Python environment is running the script.
2. `pyinstaller <name>.spec --noconfirm` — freezes the entry point named in
   `pyproject.toml`'s `[project.scripts]` to a onefile `.exe`.
3. Copies the result into `src-tauri/binaries/<exe-name>-x86_64-pc-windows-msvc.exe`
   — the exact filename Tauri's `bundle.externalBin` (in `tauri.conf.json`)
   expects.

**Tauri validates every `externalBin` entry exists on disk at build time —
even for a plain `cargo build`, not just packaging.** That's why
`src-tauri/binaries/` has two *committed, empty placeholder* files: without
them, a fresh clone can't even `cargo check` until someone has run
`plugins/build.py` once. See `src-tauri/binaries/README.md`. Before an actual
release build, always run `python plugins/build.py` to overwrite the
placeholders with real frozen executables — `tauri build` doesn't distinguish
a placeholder from a real binary, only that the file exists, so packaging
with placeholders in place produces an app whose plugins can't start.

### Resolving the bundled path from Rust

Both plugins resolve their own bundled exe path the same way — next to the
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
relevant config field (`adaptive_pathway_launch_command`, or goose's
extension `cmd` for replacement-mcp) at `uv run ...` instead, entirely
independent of this resolution.

## Adding a third plugin

See `plugins/README.md`'s "Adding a third plugin" section for the concrete
checklist. The one decision to make first: which of the two categories above
does it fall into?
