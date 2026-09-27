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
| `adaptive-pathway_rust` | **Not a binary.** Behavioural memory, a path dependency linked into the engine (`BigTinyV2/daemon`); its `record`/`forget` tools are registered through the engine's `mcp::builtin` | — (linked, not spawned) |
| `memorabilia_rust` | **Not a binary.** Factual memory, linked into the engine the same way; `memorabilia_search`/`memorabilia_read_item` | — (linked, not spawned) |
| `kitty-tools` | MCP server, 26 tools (24 on Android, which has no `lean_shell`/`lean_shell_ro`): workspace, 5 file, 3 Word, 2 Excel, 2 PDF, image reading, 4 scratchpad, 4 cache, 2 document-handle (`lean_doc_read_chunk`/`lean_doc_search`, reading the extract-once cache in `src/doc_store.rs`), shell — plus 4 visualization tools (table, SVG, chart, Mermaid) behind their own toggle. No network access | the engine, registered via `src-tauri/src/bigtiny/mcp.rs` |
| `kitty-web` | MCP server, 3 tools: `lean_web_search`, `lean_web_search_read_chunk`, `lean_web_scrape`. DuckDuckGo and Bing together (language/country honoured); Brave preferred per query when a key is configured | the engine, same registration path |
| `kitty-wasm` | MCP server, 4 tools: `execute_math_python`, `wasm_python_run`, `wasm_run_module`, `wasm_guest_status`. wasmtime + WASI, no network, no filesystem beyond explicit mounts, enforced time/memory ceilings | the engine, same registration path |

`build.py` also builds the engine itself (`BigTinyV2/daemon`, target
`bigtiny`), which Kitty attaches to on desktop and links in on Android.
`plugins/bigtiny_rust/` is the frozen V1 engine: not built, not linked.

The three MCP servers are stdio child processes on desktop and in-process over
`tokio::io::duplex` on Android, where `exec()` of app-writable binaries is
refused. Same code either way — only the transport differs, and in-process
each is configured through an explicit `InProcessConfig` rather than the
process environment.

## Two integration shapes, and why mixing them is a bug

The only process Kitty starts is the engine, and only on desktop when none is
running; even then it never kills it, because other apps share it.

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
4. Register it in `bigtiny::mcp::ensure_builtin_servers` (and its name in
   `REGISTERED_BUILTINS`) — and if it should work on Android, give the
   engine's `mcp::builtin` an arm for it too, or it will be desktop-only.
5. Give it a `cargo test` suite.

`docs/PLUGINS.md` has the full pattern.

## Building

```bash
python plugins/build.py
```

Builds every target (or just the ones named) and copies the `.exe`s, with
Tauri's target-triple suffix, into `src-tauri/binaries/`, stages the LiteRT
runtime DLLs into `src-tauri/resources/`, and records each binary's source hash
in `src-tauri/binaries/manifest.json`. `--verify-manifest` checks the committed
binaries still match their source, which CI enforces; commit a rebuilt binary
(it is a Git LFS object) with its manifest entry. Expect this to take a while:
these are release builds, and the engine alone links wasmtime, tokenizers,
rustls and the LiteRT bindings.

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
