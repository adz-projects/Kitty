# Internal plugins

Kitty ships most subsystems as **internal plugins**: independent, tested
packages, frozen to standalone Windows `.exe`s at build time and bundled via
Tauri's `externalBin` mechanism. End users need no Python, `uv`, `pip`, or
Rust toolchain — see `plugins/README.md` for the directory layout and how to
add a new plugin; this file covers the *pattern* in more depth. The BigTiny
daemon itself (Kitty's chat backend, vendored in-tree at `plugins/bigtiny/`)
follows the same freeze pipeline, even though it isn't a "plugin" in the
tool-augmentation sense — see `docs/bigtiny-backend.md`.

**As of 0.5.0 every bundled binary is Rust**, built with plain
`cargo build --release` — `kitty-tools`, `kitty-web`, `kitty-wasm`, and the
`bigtiny` daemon (which statically links the behavioral-memory engine at
`plugins/adaptive-pathway_rust/`). No PyInstaller step and no Python runtime
is involved in a release build any more; `plugins/build.py` remains only
because it owns the target-triple naming convention `externalBin` expects.

This is not a deviation from "the exception, by design" framing (CLAUDE.md's
stack section): a frozen Rust binary has *less* runtime surface than a frozen
Python one (no interpreter, no onefile self-extraction latency), so it fits the
same externalBin-bundled-plugin slot even more comfortably. The Python freeze
pipeline described below is retained because it still documents how the
retired plugins were built, and `plugins/build.py`'s `kind` field still
supports `"python"` should a future plugin need it — but no current target
uses it.

## Why frozen `.exe`s, not a bundled Python runtime

The alternative (embed CPython + pip-install deps at first run) was
considered and rejected: it still requires shipping and provisioning a full
Python environment on the user's machine, just relocated — it doesn't remove
the dependency, only hides where it lives. A frozen binary has zero runtime
dependency on anything Python-related. The same reasoning applies to
`kitty-tools`'s Rust build: a `cargo build --release` output has zero runtime
dependency on a Rust toolchain.

## The two integration shapes, and why plugins are wired differently

| | HTTP sidecar (no current example — see note) | `kitty-tools` / `kitty-web` / `kitty-wasm` |
|---|---|---|
| What it is | HTTP sidecar (FastAPI/uvicorn) | stdio MCP server |
| Who spawns it | **Kitty** (`lifecycle/adaptive_pathway_proc.rs`) | **BigTiny** |
| Who monitors it | Kitty (health loop, `ManagedProcess`) | BigTiny (its own `/api/mcp/servers` status) |
| Registered via | Config field + Kitty's own process spawn | `bigtiny::mcp::ensure_builtin_servers` (upserts BigTiny's `/api/mcp/servers`) |
| Rust wiring | `lifecycle/<name>_proc.rs`, `commands/<name>.rs`, an HTTP client module | `bigtiny/mcp.rs`, `commands/mcp_servers.rs` — no lifecycle file, no `ManagedProcess` |

> **Note (0.5.0):** the Kitty-managed-sidecar column has no current occupant.
> Its only example was the Python `adaptive-pathway` sidecar, retired once the
> behavioral-memory engine moved in-process into the BigTiny daemon. The
> BigTiny daemon itself is still Kitty-managed (`lifecycle/bigtiny_proc.rs`),
> so the pattern is live — just not for any *plugin*. The column is kept
> because the "two supervisors racing to own one child" hazard below is the
> reason the split exists, and that hazard outlives any one example.

**This split is deliberate, not incidental.** A plugin that BigTiny itself
spawns (any stdio MCP server) should *never* also get a Kitty-side
`ManagedProcess`/health-probe — that would be two supervisors racing to own
one child process. Decide which category a new plugin falls into before
writing any Rust wiring:

- **Kitty-managed process** (HTTP sidecar, background daemon Kitty talks to
  directly): follow the BigTiny daemon pattern (`lifecycle/bigtiny_proc.rs`) — a
  `lifecycle/<name>_proc.rs` with `spawn`/`ensure_running` + `probe_health`, a
  `ManagedProcess`/`DaemonHandle` field in `AppState`, commands for
  status/restart/enable.
- **BigTiny-managed MCP server** (stdio MCP server): follow the
  kitty-tools / kitty-web / kitty-wasm pattern — no lifecycle file, just an
  entry in `bigtiny::mcp::ensure_builtin_servers`'s upsert (registers/updates
  it in BigTiny's `/api/mcp/servers` by name) and a Settings toggle. BigTiny
  owns the process entirely.

### A third shape: in-process MCP server (exec()-restricted hosts)

Both shapes above assume the host can spawn a child process. That's false on
Android (10+ blocks `exec()` of anything in an app-writable directory), which
rules out `kitty-tools`' stdio subprocess the way desktop runs it. For that
case, `bigtiny_rust` has a third `TransportType::InProcess` (`models/mcp.rs`):
the server runs as a Rust library linked directly into the daemon, connected
over an in-memory duplex pipe (`mcp::client::connect_in_process`) instead of
a child's stdin/stdout — see that method's doc comment for why `rmcp`'s
client side genuinely can't tell the difference between the two, and doesn't
need to. `mcp::builtin::connect` is the registry: a DB row with
`transport: "in_process"` and `command` set to a logical name (`"kitty-tools"`,
today) is what an in-process host upserts instead of a `stdio` row pointing
at a bundled exe — everything downstream (`mcp::manager`'s enable-toggle,
connect-timeout, and evict-stale-on-failed-reconnect handling) is identical
either way.

This is why `kitty-tools/Cargo.toml` already has a `[lib]` target alongside
its `[[bin]]`: `bigtiny_rust` depends on it directly
(`plugins/bigtiny_rust/Cargo.toml`) purely for `serve_in_process`, a thin
wrapper that runs `KittyToolsServer::new().serve(stream)` — the exact same
server and tool router `main.rs` serves over stdio, just handed a different
stream. The two crates pin *different* major versions of `rmcp`
(`kitty-tools` = 2.2.0, `bigtiny_rust` = 0.9) and that's fine: the two sides
of the duplex pipe only ever exchange serialized JSON-RPC bytes, never Rust
types, so the version mismatch never has to resolve.

**Known, accepted tradeoff**: `kitty-tools/Cargo.toml`'s `[profile.release]`
(`opt-level = "z"`, fat LTO — tuned for its life as a small frozen exe) only
takes effect when cargo is building `kitty-tools` itself at the workspace
root. As a path dependency of `bigtiny_rust` it silently inherits whatever
profile that build uses instead. Not worth hoisting both crates into a
shared Cargo workspace to fix — that would reverse this file's explicit
"deliberately not a workspace member" stance above (MSRV isolation,
`panic = "abort"` not leaking from `src-tauri`), and the size/speed
difference is unlikely to matter once `kitty-tools` is linked into a full
app binary rather than shipped standalone.

Three servers are wired into `mcp::builtin` today — `kitty-tools`,
`kitty-web` and `kitty-wasm`, all listed in `mcp::builtin::BUILTIN_SERVERS`
(a test asserts every advertised name actually connects). A future one
follows the same recipe: give it a `[lib]` target with its own
`serve_in_process` entry point, add it as a dependency of `bigtiny_rust`, and
add one match arm to `mcp::builtin::connect` — nothing in `mcp::manager`,
`mcp::client`, or the DB schema needs to change.

## `kitty-web` and `kitty-wasm`: the Rust replacements for the Python plugins

Both are Rust `rmcp` servers built exactly like `kitty-tools` (stdio binary on
desktop, in-process elsewhere), and both keep their predecessors' **tool names
and response envelopes byte-identical** — adaptive-pathway keys learned
routing on the literal name string, and the JSON envelope is the model's
prompt-visible contract.

**`kitty-web`** replaces `kitty-docs-web`'s three web tools (`lean_web_search`,
`lean_web_search_read_chunk`, `lean_web_scrape`), preserving the count-tiered
Brave/DuckDuckGo behavior and the offload/keyword-index modes. Two deliberate
substitutions: `ddgs` has no Rust equivalent, so DuckDuckGo is scraped from its
own no-JS HTML endpoint (`parse_ddg_html` is written to degrade, not fail, when
that markup drifts — and `tests/live.rs` is how the drift gets noticed); and
`trafilatura` is replaced by `scraper` boilerplate-stripping plus `htmd`.
`output_format="text"` is honored by rendering the extracted Markdown to plain
text (`scrape::markdown_to_text`).

**`kitty-docs-web` is retired.** Its web tools moved to `kitty-web`; its PDF
(PyMuPDF) and Excel (openpyxl) tools moved to `kitty-tools`, implemented on
`lopdf` and `calamine` respectively (`plugins/kitty-tools/src/tools/{pdf,excel}.rs`).
The Python source stays in-tree, unbuilt, as the behavioral oracle for the Rust
ports, exactly like `replacement-mcp`/`brave-mcp-search`. Excel is read-only by
design — spreadsheet *writes* go through the existing `lean_file_*` CSV tools,
so no lossy xlsx writer crept into the small frozen `kitty-tools` binary.

**`kitty-wasm`** replaces `wasm-math-mcp` outright. That plugin was neither
WebAssembly nor a set of math tools: it exposed one tool,
`execute_math_python`, running arbitrary Python in a `multiprocessing` worker
behind an AST allowlist. `kitty-wasm` is what the name always described — a
real wasmtime + WASI sandbox. The security model is genuinely different rather
than reimplemented: instead of inspecting Python source for forbidden
constructs (a denylist over a dynamic language), the guest simply has no
capability it isn't handed — no network, no filesystem beyond explicit mounts,
and runtime-enforced ceilings on wall-clock time and memory. `import os;
os.system(...)` is not dangerous there because there is no OS to reach.

Three things about `kitty-wasm` are worth knowing before touching it:

- **The CPython guest is a pinned 26 MB download.** `guest.rs` pins the
  release tag, filename and SHA-256; a checksum mismatch is a hard failure,
  and nothing is fetched unless a caller passes `install=true`. On desktop
  the guest is bundled as an app resource (`src-tauri/resources/
  python-3.12.0.wasm`) and `ensure_builtin_servers` points `KITTY_WASM_PYTHON`
  at it, so first use is **offline** — the `install=true` download is then
  only a fallback for standalone/dev use of the server. Set `KITTY_WASM_PYTHON`
  to bundle or reuse a copy instead. Only the standard library is embedded —
  `networkx` (which the Python sandbox exposed) is pure Python and works if
  dropped into `<data dir>/site-packages`, but is not shipped.
- **Compiling the guest takes ~20s; running a script takes ~90ms.** The
  `.cwasm` module cache in `sandbox.rs` is therefore load-bearing, not an
  optimization — without it every tool call pays the 20s.
- **The desktop app runs `kitty-wasm`, not `wasm-math-mcp`.** `wasm-math-mcp`
  is retired: `ensure_builtin_servers` registers `kitty-wasm.exe` and the
  bundled guest, `wasm-math-mcp` sits in `RETIRED_BUILTINS` for stale-row
  cleanup, and its Python source stays in-tree, unbuilt, as the behavioral
  oracle (exactly like `replacement-mcp`/`kitty-docs-web`).

Anything calling into `sandbox::run_module` from async context **must** wrap it
in `tokio::task::spawn_blocking`: the synchronous WASI shim drives its host
functions with `block_on`, which panics outright on a thread already running
the tokio reactor. `server.rs` does this; the doc comment on `run_module`
explains it.

## The freeze pipeline

```
plugins/<name>/               # bigtiny included — plugins/bigtiny/
  pyproject.toml         # pinned deps + a [project.scripts] entry point (Python)
  <name>.spec            # PyInstaller onefile spec, datas/hiddenimports (Python)
  Cargo.toml             # standalone crate, own [[bin]] (Rust — kitty-tools only)
  src/... or *.py
  tests/
```

`plugins/build.py` is the **desktop lane only** — it hardcodes the Windows
target triple and emits `.exe`s, because `externalBin` sidecars are a desktop
hosting shape and Android has none by design (see §"In-process MCP servers"
below). For each of its four targets (`bigtiny`, `kitty-tools`, `kitty-web`,
`kitty-wasm` — all Rust as of 0.5.0; `python plugins/build.py` with no args
builds all four):
1. **Python** (`kind: "python"`, the default): `pip install -e ".[extras]"`
   installs the target's own pinned dependencies (plus any
   optional-dependency-group extras the target needs, e.g. adaptive-pathway's
   `sidecar` vs `mcp` groups) into whatever Python environment is running the
   script, then `pyinstaller <name>.spec --noconfirm` freezes the entry point
   named in `pyproject.toml`'s `[project.scripts]` to a onefile `.exe`.
   **Rust** (`kind: "rust"`): `cargo build --release --locked` in the
   plugin's own standalone crate — no shared workspace with `src-tauri` (see
   `plugins/kitty-tools/Cargo.toml`'s doc comment for why: MSRV isolation,
   `[profile.*]` being workspace-root-only, and feature unification all argue
   against merging it into `src-tauri`'s workspace).
2. Both paths converge: copy the resulting exe into
   `src-tauri/binaries/<exe-name>-x86_64-pc-windows-msvc.exe` — the exact
   filename Tauri's `bundle.externalBin` (in `tauri.conf.json`) expects.

`replacement-mcp`, `brave-mcp-search`, `kitty-docs-web`, and `visualizations`
are retired — all of their tools now live inside `kitty-tools` (Rust) or
`kitty-web` (Rust), see `CLAUDE.md`'s "Internal plugins" section. Their source
stays in-tree, deliberately absent from `plugins/build.py`'s `PLUGINS` dict.

For `replacement-mcp`, `brave-mcp-search`, and `kitty-docs-web`, that source
remains the oracle to re-verify the Rust ports against if a behavioral gap ever
surfaces.
`visualizations` is the exception: its Rust rebuild in
`plugins/kitty-tools/src/tools/viz/` deliberately diverges rather than
porting it — three of the four original `diagram_type`s were static clipart
(`.replace()`-templated `.svg` files with hard-coded node text unrelated to
the caller's input), which the rebuild replaced with genuinely data-driven
layout code, plus added `generate_accessible_chart` and a `tree` diagram type
the Python version never had. Do not treat `visualizations.py` as a
correctness reference for the diagram generators; it documents the
pre-rebuild behavior, not an intended target.

`generate_accessible_svg` now ships "foolproof" guarantees on top of that
rebuild: a `textLength`+`lengthAdjust` backstop so a label can never paint
outside its node/triangle, decision labels that stay inside the triangle's
wide band, row-gutter branch tags, lane-header gutters in swimlanes, and
rejection of flowchart/tree edges that would skip a layer (`VIZ_LONG_EDGE`).
Every diagram is confined to a readability width budget (`MAX_CONTENT_W`,
shepherded in `layout/mod.rs`) — layouts wrap/compress to fit, and anything
still over is rejected (`VIZ_TOO_WIDE`) rather than rendered illegibly small.
`tests/viz_invariants.rs` holds the parse-based invariants (no node overlap,
headers clear of nodes, budget respected).

`generate_accessible_mermaid` (added alongside) does **not** share those
layout guarantees: Mermaid has no Rust renderer, so the vendored Mermaid.js
runtime (`plugins/kitty-tools/src/tools/viz/assets/mermaid.min.js`, MIT — see
`mermaid.LICENSE`) is inlined into each result's HTML payload and rendered
client-side in the sandboxed iframe. Its "foolproof" contract is *guaranteed
degradation, never a blank frame*: the server rejects empty/oversized
sources, and a parse/render error shows the raw source in an error card. Cost:
about 3 MB per Mermaid result (the JS library rides along in the payload).

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
for `kitty-tools.exe`/`kitty-web.exe`/`kitty-wasm.exe`
when registering them with BigTiny.

## Adding a new plugin

See `plugins/README.md`'s "Adding a plugin" section for the concrete
checklist. The one decision to make first: which of the two categories above
does it fall into?
