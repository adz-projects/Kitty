# Pinned Versions & Verified API Surface

This file is the single source of truth for external-dependency versions and any
API-path / process-name assumptions the app relies on. A dependency bump is not
done until this file is updated and the affected code (`goosed/api.rs`,
`conflict.rs`, `starter_models.ts`, etc.) is re-verified against it.

## Goose

- **Pinned version:** **1.41.0** (CLI reports `1.41.0`; bundled Desktop `version` file `41.0.0`). Dev binary already on disk — pin to this.
- **goose binary location (dev):** `C:\Users\azolkover\AppData\Local\Programs\dist-windows\resources\bin\goose.exe` (bundled inside the installed Goose Desktop app). Also present there: `uv.exe`, `uvx.exe`. Desktop shell: `...\dist-windows\Goose.exe`.
- **Install method used for dev:** existing Goose Desktop install (no separate install needed). Block's Goose is **not on winget** (`Pressly.Goose` is an unrelated DB-migration tool).
- **⚠️ API surface — ACP, not legacy REST:** this version has **completed** the migration CLAUDE.md warned about. There is **no `goosed agent` command and no `/reply` REST API**. Server subcommands:
  - `goose serve` — **ACP (Agent Client Protocol) over HTTP + WebSocket**. Default host `127.0.0.1`, **default port `3284`**. Flags: `--host`, `--port`, `--tls[/-cert-path/-key-path]`, `--platform cli|desktop`, `--with-builtin <names>`, `--allowed-origin <origin>`, `--dangerously-unauthenticated`.
  - `goose acp` — ACP over **stdio** (`--with-builtin` only).
  - Secret-key auth **still applies**: `GOOSE_SERVER__SECRET_KEY` (the `--dangerously-unauthenticated` flag opts out). CLAUDE.md's secret-key model holds; the *transport/protocol* changes.
- **Consequence for the plan:** integration targets **ACP JSON-RPC** (`session/new`, `session/prompt`, streamed `session/update` notifications, permission/approval requests, session loading), **not** REST routes `/agent`,`/reply`,`/sessions`,`/config`. No `openapi.json` to vendor — vendor/pin the **ACP schema + method list** instead. Architecture (Rust owns I/O, Tauri events to frontend) is unchanged; the change is confined to `goosed/api.rs` + `goosed/stream.rs` — the isolation boundary CLAUDE.md designed for.
- **Streaming/reasoning surface:** ACP `session/update` **does** surface a distinct reasoning variant — `agent_thought_chunk` (separate from `agent_message_chunk`). Phase 10 renders it directly; no `<think>` splitting needed.
- **Full ACP method/transport reference:** [acp-protocol.md](acp-protocol.md) (confirmed live 2026-07-04). Transport = WebSocket `ws://127.0.0.1:<port>/acp?token=<secret>`; readiness = `GET /status` + `X-Secret-Key`.

## Ollama

- **Pinned/tested version:** **0.31.1** (confirmed live via `GET /api/version` and
  `ollama --version`, Stage-1 close-out).
- **Binary location:** `C:\Users\azolkover\AppData\Local\Programs\Ollama\ollama.exe` (detected).
- **Endpoints used:** `GET /api/version`, `GET /api/tags`, `POST /api/pull` (NDJSON), `DELETE /api/delete`.

## Stock Goose Desktop detection (Phase 1 `conflict.rs`)

- **Process name(s) to match:** `Goose.exe` (case-sensitive; already implemented in
  `conflict.rs` — this entry was left marked TBD after the fact, corrected in the
  Stage-1 close-out).

## Windows Copilot app (REMOVED — UX-simplification pass)

The hardware Copilot-key hook (`copilot.rs`, the `use_copilot_key` config field,
and the `WH_KEYBOARD_LL` chord-swallow described below) was removed by owner
decision during the UX-simplification pass — one low-level global keyboard
hook was judged not worth the complexity/risk versus a configurable hotkey,
which already does the same summon job. The section below is kept for
historical context only; none of it reflects current code.

## Windows Copilot app (Round-2 item 2 — best-effort close after swallowing the chord)

- **Appx package:** `Microsoft.Copilot`, PackageFamilyName
  `Microsoft.Copilot_8wekyb3d8bbwe` (verified on this machine 2026-07-05).
- **Process name(s):** `mscopilot.exe` (several instances seen); also `M365Copilot`
  (the Microsoft 365 Copilot app — a *different* thing, don't target it).
- **Window class:** _not yet captured_ — no Copilot window was visible during the
  probe, so its top-level window class/title is TBD. Capture it live (e.g. with
  `EnumWindows` + `GetClassName` while a Copilot window is open) before wiring the
  defense-in-depth close in `copilot.rs`; the mitigation is best-effort and must
  not block on getting this exactly right (some OEM Copilot keys are handled by
  Windows below the `WH_KEYBOARD_LL` layer and can't be fully intercepted).

## File-writing tool names (Phase 4 artifacts heuristic)

- **Two qualifying signals (Round-5, broadened):** the `deriveArtifact`
  heuristic in `chatStore.ts` now treats a tool call as an artifact producer if
  **either** (a) its title/toolName matches a write verb
  (`text_editor | write | create | edit | str_replace | insert | append | save |
  export | output | generate`) **or** (b) the output path it exposes ends in a
  recognized artifact extension (`csv/tsv/xlsx/xlsm/doc(x)/ppt(x)/md/markdown/
  json/jsonl/yaml/py/txt/html/xml/pdf/rtf/odt/ods/odp/ipynb/sql/toml`). The path
  still comes from `rawInput.path` (also `file_path`, or `paths[0]`). Explicit
  reads carrying a path (`rawInput.command` ∈ `view/read/list/open/cat/show/
  inspect/search/find/glob/grep`) are excluded — this also fixed a latent false
  positive where a plain `text_editor` "view" registered as a bogus artifact.
  Heuristic still errs toward false negatives, never fabricates (CLAUDE.md).

## Where files land — session cwd vs. goose's default (Round-5)

- **The per-chat working directory is honored.** Kitty passes each session's cwd
  (`Documents/Kitty/chats/<id>/`, from `resolve_cwd`) to `session/new`, and goose
  respects it: probed live (2026-07-06) `echo %CD%` from the shell tool returned
  the exact chat folder, and a relative-path `text_editor` write (`notes.txt`)
  landed there. So relative writes go to the right place per chat.
- **But the model tends to use goose's absolute default for some outputs.**
  goose's built-in default working directory is `~/Documents/Goose`; when asked
  to "export a docx," the model was writing to an absolute `~/Documents/Goose/…`
  path instead of a relative one, so exports piled up there. That's a model/goose
  behavior, not a Kitty cwd bug (the cwd is already correct).
- **Mitigation (soft nudge):** `goosed_env()` sets `GOOSE_MOIM_MESSAGE_TEXT`
  (consumed by goose's bundled `tom` / "Top Of Mind" platform extension, which
  injects it into every turn) instructing the model to save files into the
  current working directory using relative paths, not an absolute
  `~/Documents/Goose` path. Confirmed live the model receives it (quoted the text
  back verbatim) and chose a relative path afterward. It's a prompt-level nudge,
  not a hard guarantee — a model can still ignore it and write elsewhere.

## Artifact writes by provider — Round-5 diagnosis

- **Kitty imposes no restriction on what a provider can write.** There is no
  trust/tier gate on tool execution anywhere in this codebase; `is_trusted` /
  `NetworkTier` only drive UI (badges, the untrusted-attach warning). goosed
  executes tools; Kitty neither sandboxes nor filters them by file type.
- **File-writing tools are always available.** Probed live (2026-07-06): a fresh
  `session/new` already has the `developer` platform extension enabled
  ("Write and edit files, and execute shell commands") in addition to the
  `computercontroller` builtin Kitty force-adds. So `text_editor` (text writes)
  and `shell` are present in every session with no extra wiring — Kitty does
  **not** need to force-add `developer`.
- **Text formats work and appear in the Artifacts pane** (csv, md, json, py,
  txt, html, etc.): the model writes them via `developer`'s `text_editor`, whose
  tool call exposes `rawInput.path`, so `deriveArtifact` detects them.
- **Binary Office formats (xlsx/docx/pptx) are a goosed-environment concern,
  outside Kitty's control:** `text_editor` only writes text, so the model must
  generate these via `shell` running Python (`openpyxl` / `python-docx` /
  `python-pptx`). If those libraries aren't present in the environment goosed's
  shell runs in, the write fails — nothing Kitty can fix client-side. A
  shell-executed write's tool call is `{command: "..."}` with no structured
  `path`, so `deriveArtifact`'s tool-metadata derivation still misses it — but
  (Round-7 item 5) the Artifacts pane also disk-scans the session's working
  directory (`refreshArtifactsFromDisk`/`list_directory`), so a successful
  shell-produced binary now does surface there, just via the disk scan rather
  than the tool-call path.
- **Chat ("thought-partner") mode allows tools, scoped to the chat folder
  (Round-5, owner decision — supersedes Round-4's blanket auto-reject; made
  tri-state in Round-7 item 3):** chat mode still forces `approve` so every
  tool call surfaces as a permission request `decideChatApproval`
  (`stores/chat/approvalUtils.ts`) decides. It returns one of three
  decisions — `allow`/`reject`/`prompt` — not just a boolean: a path-based file
  op resolving inside the session's cwd (the `Documents/Kitty/chats/<id>/`
  folder) is `allow`; one resolving outside it, or a shell command matching
  `isSecuritySensitiveCommand` (ssh/scp/sudo/`rm -rf`/chmod/curl -o/etc.), is
  `prompt` — queued to `pendingApprovals` for the user to decide, same as
  agentic mode, rather than auto-rejected outright; anything else (notably most
  `shell` calls, which is how docx/xlsx get produced via Python) is `allow` and
  runs with cwd = the chat folder. **Soft boundary, not a sandbox:** shell isn't
  confined, so a command could still reach outside the folder; the path check
  and sensitive-command check only hard-confine what Kitty can actually
  inspect. This is what lets a thought-partner-mode session export a docx
  (which previously hit "Tool use is off in chat mode — declined") while still
  surfacing a human decision point for genuinely risky commands instead of
  silently running them.

## Installer URLs & hashes (Phase 7; Goose auto-install added in the wizard redesign)

- **Ollama Windows installer:** `https://ollama.com/download/OllamaSetup.exe`
  (Inno Setup; hands off to its own UI/UAC — verify a silent flag before enabling
  unattended install). Wired in `src-tauri/src/wizard.rs`.
- **Goose:** _confirmed (Stage-1 close-out): there is no Windows `.exe`/`.msi`
  installer at all_ — the [releases page](https://github.com/aaif-goose/goose/releases/latest)
  (org renamed from `block/goose`; GitHub still redirects the old path) only
  publishes zip archives for Windows:
  - `goose-x86_64-pc-windows-msvc.zip` (~78 MB) — the bare CLI/`goose serve`
    binary. **This is the one Kitty needs and installs automatically.**
  - `goose-x86_64-pc-windows-msvc-cuda.zip` — same, CUDA-enabled build. Not
    used by the auto-install (the plain build is the safe default).
  - `Goose-win32-x64.zip` / `Goose-win32-x64-cuda.zip` / `Goose.zip` — the full
    **Goose Desktop** Electron app. **Never auto-install this one** — it's
    the separate GUI product `conflict.rs` already detects and warns about
    (`Goose.exe` process name); installing it would create the exact conflict
    Kitty is designed to flag.
  - **Wizard redesign (current behavior):** since there's no installer
    executable to silently run, `wizard::install("goose")`
    (`src-tauri/src/wizard.rs`) instead resolves the CLI zip's real download
    URL from the GitHub Releases API by exact asset name
    (`GOOSE_CLI_ASSET_NAME`), downloads it, extracts it via the `zip` crate
    into `%LOCALAPPDATA%\Kitty\goose\`, and persists the extracted
    `goose.exe` path as `Config.goose_binary_override` — which
    `lifecycle::goosed::locate_goose` now checks first, before the env var /
    Goose Desktop bundle path / bare PATH fallbacks. The wizard's Detect step
    calls this from a real "Install" button; a "I already have it" fallback
    (native `.exe` file picker) sets the same override manually for a user
    who's already got Goose somewhere non-standard.

## Wizard redesign: local-vs-API-key fork, `ollama_enabled` (2026-07-11)

- **The wizard's first screen now forks**: "Run models on this computer"
  (existing Detect/Configure/First-model flow, Ollama+Goose auto-install) vs.
  "Use my own API key" (new step reusing `Providers.tsx`'s save/activate
  infra — Anthropic/OpenAI/OpenRouter/Custom, base URLs from the shared
  `src/lib/provider_defaults.ts`). First-party API-key providers created this
  way are marked `is_trusted: true` immediately (owner decision — no scary
  ⚠ badge for a key the user just pasted on purpose); `custom_openai` stays
  untrusted by default, same as adding one from Settings.
- **`Config.ollama_enabled`** (default `true`, so pre-existing installs are
  unaffected): set explicitly by the wizard's fork. `false` hides Settings →
  "Ollama Models" and the "Ollama" option in Add Provider's type picker, and
  `start_stack`/`compute_status` (`config::providers::requires_local_ollama`)
  stop trying to reach Ollama at all. Settings → Advanced has an "Enable &
  install Ollama" action that flips it back on and runs the same install path
  the wizard uses.
- **`validate_setup` command** (`src-tauri/src/commands/setup.rs`) is the
  single source of truth for "is this setup actually ready to chat" — checks
  the active provider has a model (and, for remote types, a stored key) plus
  a fresh `compute_status`. Powers the wizard's Done-step summary and its
  soft Finish-anyway gate (never a hard block), and Setup & Repair's re-check.
- **Adaptive Pathway auto-install (near-term bridge):** the wizard now
  attempts a best-effort `pip install adaptive-pathway[sidecar]` if the
  console scripts aren't already resolvable (`wizard::install_adaptive_pathway`).
  Failure is non-fatal — the extension just stays `Down`, same graceful
  degradation as always, with a manual retry in Settings → Advanced. The
  real, owner-specified target is bundling a standalone sidecar executable as
  a Tauri `externalBin` sidecar (no Python dependency at all) — not yet
  built; this pip-based path is an explicit bridge until that lands.

## Starter models (Phase 7 `src/lib/starter_models.ts`)

- **Curated VRAM-tiered list** (re-verified on ollama.com, replacing the
  earlier ≤4B `llama3.2`/`qwen2.5`/`gemma2` set now that `gemma4`/`qwen3.5`
  are current): `gemma4:e2b` (7.2GB — 4-8GB VRAM), `qwen3.5:4b` (3.4GB — 8GB
  VRAM), `gemma4:e4b` (9.6GB — 8GB VRAM), `qwen3.5:9b` (6.6GB — 16GB VRAM).
  Re-verify tags/sizes again before release if enough time has passed for the
  lineup to have moved again.

## Reasoning-capable models (Phase 10 `src/lib/reasoning_models.ts`)

- **Name patterns → supports-reasoning:** `think` (lfm2.5-thinking, *-thinking),
  `reason`, `deepseek-r1`, `qwq`, `magistral`, OpenAI `o1/o3/o4`, bare `r1`.
- This table only drives the *predictive* thinking indicator. The reasoning panel
  is **content-driven** (shown whenever a model actually emits `agent_thought_chunk`),
  so models that reason but don't match a pattern (e.g. `gemma4:e2b`, which emits
  thoughts here) still get a panel. ACP surfaces reasoning as a distinct
  `agent_thought_chunk` — no `<think>` tag splitting needed (see acp-protocol.md).
- **Rust port**: `plugins/bigtiny_rust/src/agent/reasoning_models.rs`, same pattern
  table, same "re-verify on model updates" caveat. Two independent copies (this file
  is the frontend's) is an accepted duplication-drift risk — see that module's doc
  comment.

## Thought-seeding assistant-prefill support (adaptive-pathway behavioral memory)

- `Provider::supports_assistant_prefill` (`plugins/bigtiny_rust/src/provider/base.rs`)
  gates whether the pathway engine's per-turn recall is seeded as a trailing
  `<think>` assistant-turn prefill (`PathwayEngine::recall_thought_seed`) instead of
  the default `[Working assumptions about you]` system-block injection.
- **Anthropic**: `true` unconditionally — the Messages API's trailing-assistant-
  message continuation is documented, protocol-native behavior, not something that
  needs per-deployment verification.
- **OpenAI-compatible (Ollama, OpenRouter, custom endpoints)**: `false` by default,
  gated behind `ProviderConfig::experimental_prefill` (an explicit per-provider
  opt-in, same pattern as the `remote`-tier "I understand" warning). Whether a
  trailing partial assistant message actually continues generation — versus
  erroring, or the server silently starting a fresh turn and ignoring it — depends
  on the specific server/chat-template combination and is **not** part of the
  OpenAI chat-completions spec. **Not yet verified against the pinned Ollama
  version** (see the top of this file) for any model. Before flipping this default
  on for Ollama: pull a reasoning-capable model (e.g. a `-thinking` variant),
  enable `experimental_prefill` for that provider, start a session, and confirm in
  the transcript that the seeded `<think>` content is actually continued from
  (not echoed back, not answered as a fresh user turn, and no raw `<think>` tag
  leaking into the visible reply) — then record the verified model/version
  combination here.

## Behavioral-memory recall framing (adaptive-pathway)

Both render paths — the `[Working assumptions about you]` system block
(`antisycophancy::render_block`, what local Ollama actually sees) and the
`<think>` thought seed (`recall::render_reflection_block`, Anthropic-only
today per the section above) — deliberately frame recalled beliefs as a
*provisional prior to test the current request against*, never as a profile
to conform to. This is a behavioral contract, not styling:

- The seed template previously ended "I'll let that inform my tone without
  stating it outright" — a conformity instruction — and rendered only the
  fact list, dropping `[Worth testing this turn]` / `[Where I'm unsure]` /
  `[Check yourself]`. That made the seeded path strictly *more* sycophantic
  than the block it replaces. `PathwayEngine::turn_signals` is now shared by
  both paths so they cannot diverge on which signals they carry.
- `tests/recall_engine.rs::neither_render_path_tells_the_model_to_conform`
  and `::the_thought_seed_carries_the_same_signals_as_the_system_block` fail
  the build if either property regresses.
- Section header strings are load-bearing:
  `recall::truncation_order()` drops sections by `starts_with` on them.
  Renaming a header without updating that list silently disables the
  350-token truncation path.

## Internal plugins & build tooling (`plugins/`)

- **Python:** 3.11 (matches the `.pyc` cache tags already present in the
  vendored Adaptive Pathway source — re-verify against whatever interpreter
  actually runs `plugins/build.py` at release time).
- **PyInstaller:** installed on demand by `plugins/build.py`
  (`pip install pyinstaller`, no version pinned yet — pin here once a
  specific version is confirmed to freeze both plugins cleanly).
- **Adaptive Pathway** (`plugins/adaptive-pathway/`): freezes the `sidecar`
  extra only (fastapi/uvicorn/numpy/aiosqlite/sqlalchemy/pyyaml/mmh3) — the
  `full` extras (onnxruntime/hdbscan/bertopic) are excluded from the frozen
  binary since Kitty doesn't surface the clustering/topic features that need
  them; revisit if that changes.
- **replacement-mcp** (`plugins/replacement-mcp/`): freezes `lean_mcp.py`'s
  `main()` — deps: fastmcp, httpx, trafilatura, ddgs, openpyxl,
  python-docx, pypdf, pyyaml (see `plugins/replacement-mcp/pyproject.toml`).
  The search dep is **`ddgs` (>=9.0), not `duckduckgo-search`** — the latter
  was renamed, and its last releases (8.x) return zero results for every
  query against DuckDuckGo's current backend, which surfaced in chat as
  `lean_fallback_web_search` reporting "No results found" no matter what was
  asked. Verified working on ddgs 9.11.3; the API (`DDGS().text(query,
  max_results=…)` → `title`/`href`/`body`) is unchanged from 8.x.
- Both freeze to `src-tauri/binaries/<name>-x86_64-pc-windows-msvc.exe` and
  are declared in `src-tauri/tauri.conf.json`'s `bundle.externalBin`. The
  committed files at that path are **empty placeholders** (satisfy Tauri's
  build-time existence check for local `cargo build`) until
  `python plugins/build.py` overwrites them with real frozen executables —
  see `src-tauri/binaries/README.md`.

## `lean_web_search` merge — `brave_mcp_search` + `lean_fallback_web_search` retired

- `brave_mcp_search` (Rust, `kitty-tools`, gated on `BRAVE_API_KEY`) and
  `lean_fallback_web_search` (Python, `kitty-docs-web`, DuckDuckGo via
  `ddgs`) are both retired, replaced by one merged, count-tiered tool pair —
  `lean_web_search` / `lean_web_search_read_chunk` — hosted in
  `kitty-docs-web` (Python). The tool no longer requires the model to choose
  an engine: `count <= 5` (default) tries Brave first if configured, falling
  back to DuckDuckGo only on Brave failure; `6 <= count <= 10` queries both
  engines concurrently for broader coverage; `count > 10` does the same
  dual-engine fetch but offloads the full result set to a temp file and
  returns a compact, deterministic keyword index (frequency-based, not LSA —
  no ranking dependency needed at this scale) instead of full detail —
  `lean_web_search_read_chunk` fetches full detail for chosen ids afterward.
- **Hosting moved from Rust to Python**: DuckDuckGo has no Rust crate
  equivalent (the alternative was hand-rolling and maintaining an HTML
  scraper against DuckDuckGo's lite endpoint), while Brave's call is a
  plain JSON GET — trivial to host in Python alongside `ddgs` instead. So
  `kitty-tools` loses its network surface entirely (dropped the `reqwest`/
  `url`/`rand` dependencies, now purely local-machine tools + viz), and
  `src-tauri/src/bigtiny/mcp.rs`'s `BRAVE_API_KEY` env-var wiring moved from
  the `kitty-tools` upsert block to the `kitty-docs-web` upsert block.
- **Expected fallout, not a regression:** renaming/removing these two tool
  names resets adaptive-pathway's learned Thompson-bandit routing state for
  them (it hashes the literal tool-name string — see
  `plugins/kitty-tools/tests/protocol.rs`'s header note). Both new tool
  names start cold.
- The new DDG path must not reproduce `lean_fallback_web_search`'s known
  "No results found" failure mode noted above (the `ddgs` 8.x→9.x rename);
  it's covered by fixture/mock-based tests in
  `plugins/kitty-docs-web/tests/test_web_search.py`, never live network.

## Visualization tools rebuilt — static clipart replaced, `generate_accessible_chart` added

- **Root cause fixed:** three of the four `generate_accessible_svg`
  `diagram_type`s (`flowchart`, `swimlane`, `journey_map`) were `include_str!`
  static `.svg` assets customized only by a `title`/`description`
  `str::replace` — the node text (an HTTP-auth flow, an e-commerce checkout, a
  SaaS onboarding funnel) never reflected the caller's actual input. The Rust
  port had also dropped every per-field JSON-Schema description the Python
  original (`plugins/visualizations/visualizations.py`) carried, leaving only
  `diagram_type` documented. Together these meant a 27B local model could not
  produce anything but generic output from these tools.
- **Rebuilt as genuinely data-driven layouts** in
  `plugins/kitty-tools/src/tools/viz/layout/`: `single_lane` (row-wrapping
  instead of the old fixed 880px-wide viewBox, which silently clipped past
  ~5 steps), `flowchart` (layered-DAG via longest-path-from-roots + a
  barycenter-ish ordering sweep, branches/merges, YES/NO tags on decision
  edges), `tree` (new `diagram_type` — Reingold-Tilford-lite), `swimlane`
  (lanes from the caller's `lane` values), `journey_map` (bands composed
  from whichever of `subtitle`/`sentiment`/`pain` are actually present). All
  five share one node-sizing/wrapping engine (`layout::size_node`, backed by
  a static Helvetica advance-width table — no embedded font — in `text.rs`)
  so a long label wraps and the canvas grows instead of clipping.
  `assets/{flowchart,swimlane,journey_map}.svg` were deleted.
- **New tool: `generate_accessible_chart`** (`bar`/`horizontal_bar`/`line`/
  `grouped_bar`), the crate's first chart capability — series differentiated
  by fill pattern/dash style rather than color alone (grayscale/color-blind
  accessible), with a hidden `<table class="sr-only">` alongside the SVG for
  screen readers. Starts cold in adaptive-pathway's Thompson bandit, same
  accepted cost as the `lean_web_search` merge above —
  `generate_accessible_table`/`generate_accessible_svg` keep their existing
  names and bandit state.
- **Schema rewritten for a small model's benefit:** every field across
  `AccessibleTableRequest`/`AccessibleSvgRequest`/`AccessibleChartRequest`
  now carries a description; `diagram_type`/`chart_type`/step `type` are real
  Rust enums kept intentionally **undocumented per-variant** (a doc comment
  on an enum *variant* flips schemars 1.x from a flat
  `{"type":"string","enum":[...]}` to `oneOf`-of-`const`, which
  llama.cpp/Ollama grammar-constrained decoding handles far less reliably —
  `tests/schema.rs` asserts the flat form so this can't regress silently);
  each tool description carries a compact worked-example JSON call.
  `steps`/`categories`/`series` are now required with no silent fallback —
  the old `single_lane` behavior of substituting a canned "Ingest Data" demo
  pipeline whenever `steps` was omitted is gone.
- **Escaping**: all user text now reaches SVG/HTML only through
  `render::svg`/`render::table`'s escaping primitives, replacing the crate's
  prior "unescaped, bounded by the sandboxed iframe's opaque origin" policy.
  Also fixed a real bug in the old two-step
  `.replace("__TITLE__",t).replace("__BODY__",b)` template substitution: a
  title containing the literal text `__BODY__` would get re-scanned and
  spliced with the body content. `escape::render_template` does a single
  pass over the template instead.

## Diagram foolproofing + `generate_accessible_mermaid`

- **No-overlap guarantees** (`plugins/kitty-tools/src/tools/viz/`): a
  `textLength`+`lengthAdjust="spacingAndGlyphs"` backstop on node labels means
  a label can never paint wider than its box regardless of the user's font;
  decision (triangle) nodes are sized/placed so text stays in the wide lower
  band (no apex spill); YES/NO branch tags move into the row gutter (never on
  a node); swimlanes reserve a lane-header gutter so tall nodes can't overdraw
  the lane name; and flowchart/tree edges that skip a layer are rejected
  (`VIZ_LONG_EDGE`) instead of crossing intermediate nodes.
- **Readability budget**: every diagram must fit `MAX_CONTENT_W` (~1100px;
  ~1500 for swimlane/journey) or it would render illegibly small when the
  iframe scales it to fit. Layouts wrap/compress (per-layer gap + node-width
  `size_node_capped`) to meet it; anything still over returns `VIZ_TOO_WIDE`
  with a hint. `wrapper.html` now uses `overflow-x: auto` as a last-resort
  safety net instead of `hidden`. `tests/viz_invariants.rs` pins the invariants.
- **New tool: `generate_accessible_mermaid`** — renders arbitrary Mermaid DSL
  (flowchart/sequence/class/state/ER/gantt/journey/pie/mindmap/gitGraph/…).
  No Rust Mermaid renderer exists, so the MIT-licensed `mermaid.min.js`
  (v10.9.1, vendored in `assets/` with `mermaid.LICENSE`) is inlined into each
  result's HTML payload and rendered client-side in the sandboxed iframe
  (`securityLevel: 'strict'`, source `<\/`-escaped, accessibility title/desc
  wired through). Its contract is *guaranteed degradation, never a blank
  frame*: server rejects empty/oversized sources (`VIZ_EMPTY_MERMAID`/
  `VIZ_MERMAID_TOO_LARGE`), and a parse error shows the raw source in an error
  card. **Cost**: ~3 MB per Mermaid result (the JS library rides in the
  payload) and ~3 MB in the frozen exe. Starts cold in the bandit (new name),
  the same accepted cost as `generate_accessible_chart`. This tool does **not**
  promise the layout invariants above — Mermaid controls its own layout.
- **Grayscale polish**: softened node/vector/pill/tag styling in
  `assets/defs.svg` (thin `#c9c9d1` strokes, weaker shadow, rounder corners)
  plus tighter `GAP_X`/`GAP_Y`, kept dark-on-light for accessibility.

- See `docs/PLUGINS.md` for why `visualizations.py` is no longer treated as
  a correctness oracle for this rebuild.

## kitty-docs-web retired — Excel/PDF to kitty-tools, web to kitty-web

- **`kitty-docs-web` (Python) is retired.** Its three web tools
  (`lean_web_search`/`lean_web_search_read_chunk`/`lean_web_scrape`) were
  already served by the Rust `kitty-web` process (see the "`lean_web_search`
  merge" section above); its PDF (PyMuPDF) and Excel (openpyxl) tools now
  live in `kitty-tools` (Rust). The desktop server registration, its config
  keys, commands, Settings card, `build.py` entry and `externalBin` bundling
  are all removed; its source stays in-tree as the behavioral oracle.
- **New `kitty-tools` tools** (always-on): `lean_excel_inspect`,
  `lean_excel_read_rows`, `lean_pdf_read_text`, `lean_pdf_read_outline`.
  `lean_excel_write_rows` is **deliberately dropped** — spreadsheet writes go
  through the existing `lean_file_*` CSV tools instead of reintroducing a
  lossy xlsx writer into the small frozen binary (see `docs/PLUGINS.md`).
- **Deps**: `kitty-tools` gains `calamine` (Excel read, with the `dates`
  feature for datetime cells → ISO strings) and `lopdf` (pure-Rust PDF).
  `-openpyxl`/`-pymupdf` are no longer bundled.
- **`BRAVE_API_KEY` owner changed**: it now attaches to the `kitty-web`
  server's env in `bigtiny::mcp::ensure_builtin_servers` (it leaves the
  retired kitty-docs-web block). Keyring id + `set_brave_mcp_search_*`
  commands unchanged.
- **Accepted divergences** (documented, same spirit as the DDG-scrape/`htmd`
  substitutions):
  - **PDF text layout**: `lopdf` does plain per-page `extract_text` with no
    PyMuPDF markdown/layout pass, so text run/column order can differ from
    the Python output. Outlines (`get_toc`) produce the same `{level, title,
    page}` triples.
  - **Excel reads `.xls`/`.ods` too** (broader than openpyxl), and
    integer-valued cells serialize as JSON integers (`1`, not `1.0`) to match
    openpyxl's Python `int`.
  - **`kitty-web` `lean_web_scrape` honors `output_format="text"`** by
    rendering the extracted Markdown to plain text (`scrape::markdown_to_text`),
    instead of silently ignoring the parameter.
- **Tool-name fallout**: `lean_excel_*`/`lean_pdf_*` are new names that seed
  adaptive-pathway's Thompson bandit cold; no existing name was renamed
  (adaptive-pathway hashes the literal tool-name string — see
  `plugins/kitty-tools/tests/protocol.rs`).
