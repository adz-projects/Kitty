# Kitty internal plugins

Subsystems that ship as part of Kitty but are maintained as independent,
testable crates rather than living inside `src-tauri/`.

**Everything here is Rust.** `build.py` builds each with a plain
`cargo build --release` and copies the result into `src-tauri/binaries/`,
where Tauri's `externalBin` mechanism bundles it. End users need no runtime of
any kind — no Python, no Rust toolchain. The PyInstaller freeze path this
directory once used is gone, along with every Python plugin; `build.py` is
still a Python script only because it owns the target-triple naming convention
`externalBin` expects.

## Current plugins

| Plugin | Integration surface | Managed by |
|---|---|---|
| `bigtiny_rust` | HTTP+SSE daemon — Kitty's chat backend, and the host for everything below | Kitty: `lifecycle/bigtiny_proc.rs` (desktop) or `lifecycle/bigtiny_embedded.rs` (Android) |
| `adaptive-pathway_rust` | **Not a binary.** A path dependency statically linked into the daemon; its `record`/`forget` tools are registered through `bigtiny_rust::mcp::builtin` | — (linked, not spawned) |
| `kitty-tools` | MCP server, 24 tools: shell, workspace, 5 file, 3 Word, 2 Excel, 2 PDF, 4 scratchpad, 4 cache, 2 document-handle (`lean_doc_read_chunk`/`lean_doc_search`, reading the extract-once cache in `src/doc_store.rs`) — plus 3 visualization tools (accessible table/chart/Mermaid) gated by `KITTY_VIZ_ENABLED`. No network access | BigTiny, registered via `src-tauri/src/bigtiny/mcp.rs` |
| `kitty-web` | MCP server, 3 tools: `lean_web_search`, `lean_web_search_read_chunk`, `lean_web_scrape`. DuckDuckGo always; Brave preferred per-query when `BRAVE_API_KEY` is set | BigTiny, same registration path |
| `kitty-wasm` | MCP server, 4 tools: `execute_math_python`, `wasm_python_run`, `wasm_run_module`, `wasm_guest_status`. wasmtime + WASI, no network, no filesystem beyond explicit mounts, enforced time/memory ceilings | BigTiny, same registration path |

The three MCP servers are stdio child processes on desktop and in-process over
`tokio::io::duplex` on Android, where `exec()` of app-writable binaries is
refused. Same code either way — only the transport differs.

## Two integration shapes, and why mixing them is a bug

Kitty only manages a process it **directly spawns and monitors** — that is the
BigTiny daemon, and nothing else.

The MCP servers are BigTiny extensions like any other. Kitty's entire
involvement is keeping their registration rows in the daemon's
`/api/mcp/servers` accurate: the command path pointed at the current install's
bundled exe, the `enabled` flag in sync with Settings, and the per-server tool
timeout. Kitty never spawns or supervises them. Treating one shape as the other
is what `docs/PLUGINS.md` warns about at length.

## Directory shape

```
plugins/
  <name>/
    Cargo.toml     # standalone crate, own [[bin]] — deliberately NOT a
                   # workspace member of src-tauri (see kitty-tools/Cargo.toml)
    src/
    tests/
  build.py         # builds every target -> src-tauri/binaries/
  README.md        # this file
```

## Adding a plugin

1. Create `plugins/<name>/` as a standalone crate with a `[[bin]]` matching the
   exe name.
2. Add it to `PLUGINS` in `build.py` with `kind: "rust"`.
3. Add that binary name to `bundle.externalBin` in
   `src-tauri/tauri.conf.json`.
4. Register it in `bigtiny::mcp::ensure_builtin_servers` — and if it should
   work on Android, add it to `bigtiny_rust::mcp::builtin` too, or it will be
   desktop-only.
5. Give it a `cargo test` suite.

`docs/PLUGINS.md` has the full pattern.

## Building

```bash
python plugins/build.py
```

Builds all four targets and copies the `.exe`s, with Tauri's target-triple
suffix, into `src-tauri/binaries/`. Expect this to take a while: these are
release builds, and `bigtiny_rust` alone links wasmtime, tokenizers, rustls and
the LiteRT bindings.

## Retired

`replacement-mcp`, `brave-mcp-search`, `visualizations`, `kitty-docs-web`,
`wasm-math-mcp`, the Python `adaptive-pathway` sidecar and its MCP proxy, and
the original Python `bigtiny` daemon have all been **deleted**. Their tools
live on in `kitty-tools` / `kitty-web` / `kitty-wasm`, and their server rows are
actively removed from the daemon on sync by `RETIRED_BUILTINS` in
`src-tauri/src/bigtiny/mcp.rs`.

These trees were kept in-tree for a while as behavioral oracles for the Rust
ports. That is over — the ports are verified and shipping, and the source is
recoverable from git history if a behavioral question ever needs settling.
