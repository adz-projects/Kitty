# Pinned Versions & Verified API Surface

This file is the single source of truth for external-dependency versions and any
API-path / process-name assumptions the app relies on. A dependency bump is not
done until this file is updated and the affected code (`src-tauri/src/bigtiny/`,
`src/lib/curated_models.ts`, `src/lib/reasoning_models.ts`, etc.) is
re-verified against it.

Sections marked **HISTORICAL** describe code that no longer exists; they are
kept only where they explain why something current is shaped the way it is.
The Goose/goosed, Goose Desktop conflict-detection, Ollama-installer and
`starter_models.ts` sections were removed outright — Goose is not part of this
app, and every file they referenced is gone.

## Ollama — **HISTORICAL, no longer a dependency**

Kitty managed an Ollama process through Phase 2a. Phase 2b removed it
entirely: there is no detection, no install, no spawn, no supervision, and no
`/api/pull`. Local inference is **LiteRT** linked into the BigTiny daemon
(embeddings on both platforms, compaction summarization on Windows only; there
is no local chat), and
models arrive through the HuggingFace downloader (`src-tauri/src/models/`).

What survives is one thing only: `provider_type: "ollama"` as a **remote**
endpoint dialect, for a server the user runs themselves. That dialect surface
is `provider/sampling.rs`, `openai_compat.rs`'s `top_k`/`min_p` wire gate,
`bigtiny/providers.rs`'s `provider_dialect`, and `ProviderForm`'s matching
fields — not a process lifecycle. Do not re-add one.

Retained for the record: the version this was verified against was **0.31.1**
(`GET /api/version`), and the endpoints used were `GET /api/version`,
`GET /api/tags`, `POST /api/pull` (NDJSON), `DELETE /api/delete`.

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

## Reasoning-capable models (Phase 10 `src/lib/reasoning_models.ts`)

- **Name patterns → supports-reasoning:** `think` (lfm2.5-thinking, *-thinking),
  `reason`, `deepseek-r1`, `qwq`, `magistral`, OpenAI `o1/o3/o4`, bare `r1`.
- This table only drives the *predictive* thinking indicator. The reasoning panel
  is **content-driven** (shown whenever a model actually emits `agent_thought_chunk`),
  so models that reason but don't match a pattern (e.g. `gemma4:e2b`, which emits
  thoughts here) still get a panel. ACP surfaces reasoning as a distinct
  `agent_thought_chunk` — no `<think>` tag splitting needed.
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

## Scraped search engines — verified surface (2026-09-11)

`kitty-web` scrapes two engines. Neither is a versioned API, so this records
what was actually observed rather than what is documented anywhere.

### DuckDuckGo — `https://html.duckduckgo.com/html/` (POST `q`, `kl=wt-wt`)

- **Challenges under concurrent load, probabilistically.** Six sequential
  queries with no delay returned three real pages and three challenges; a
  six-way parallel run later returned challenges for *all six*. The challenge
  persists for a while after the burst stops — an IP that has been hammered
  keeps getting refused for minutes.
- **The challenge is served with `HTTP 202`**, not an error status, so
  `StatusCode::is_success()` accepts it. This is the whole reason
  `classify_scrape_status` gates on a literal `200`.
- Challenge page markers: `anomaly.js`, `cc=botnet`, `challenge-form`, and the
  visible string "Unfortunately, bots use DuckDuckGo too." `ddg_challenge_marker`
  checks the first three, **only for a response that parsed to zero results** —
  so it can never demote a working page.
- Results markup unchanged: `div.result` / `div.web-result`, `a.result__a`,
  `a.result__snippet`, with hrefs wrapped as `//duckduckgo.com/l/?uddg=<encoded>`.
- `lite.duckduckgo.com/lite/` is challenged identically — not an alternative.
  The internal JSON endpoint `links.duckduckgo.com/d.js` returns an `is506`
  block signal rather than results, so it is not a bypass either.

### Bing — `https://www.bing.com/search` (GET `q`, `setlang=en`)

- No challenge observed, including during the parallel runs where DuckDuckGo
  refused every request.
- Markup: `li.b_algo` containers; title in `h2 a[href]`; snippet in
  `.b_caption p.b_lineclamp2`; display URL in `cite`. Ads carry `b_ad` in the
  container class.
- **Result URLs are wrapped**: `https://www.bing.com/ck/a?…&u=a1<base64url>`.
  The `a1` prefix is a format tag; strip it and base64url-decode (no padding)
  for the real target. `unwrap_bing_redirect` falls back to the wrapper URL,
  and the domain to `<cite>`, when the payload doesn't decode.
- **There is no keyed alternative.** Microsoft retired the standalone Bing
  Search API, so this engine is scrape-only — verify before assuming an API
  route exists.

### Engines evaluated and rejected

Mojeek (captcha), Ecosia (HTTP 403), `search.brave.com` HTML (connection
refused), Startpage (no result markup without JS), Stract (404), and four
public SearXNG instances (JSON format disabled / 429 / 403 / bot-wall).
Marginalia's key-free API (`api.marginalia.nu/public/search/{q}`) works and
needs no key, but its index is small and long-tail-only and its results are
**CC-BY-NC-SA 4.0** — a non-commercial licence not worth taking on.

### Pacing

`plugins/kitty-web/src/ratelimit.rs` spaces requests process-wide — 800ms for
DuckDuckGo, 600ms for Bing, 2 back-to-back after idle, queue refused past 20s.
Process-wide is the correct scope because BigTiny holds one `kitty-web` client
for the whole daemon (`MCPServerManager::servers`), so the main agent and every
`call_specialist` delegate share it.

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
  - **Android attachments are `content://` URIs, not paths.** The document
    picker (`ACTION_OPEN_DOCUMENT`, behind Tauri's dialog plugin) returns a
    URI; nothing in Rust can open one, and its last path segment is the
    storage provider's internal document id (`msf%3A1000000123`), not a
    filename. Every downstream decision was therefore wrong on Android: the
    image check tested a document id against a list of extensions,
    `read_file_any` had nothing to open, and the model received a URI it
    correctly reported having no tool for (observed verbatim in a reasoning
    trace: *"I don't have a tool to resolve content:// URIs"*). Resolved
    through the ContentResolver in `KittyPlugin.copyContentUri` — display name
    from `OpenableColumns.DISPLAY_NAME`, bytes from `openInputStream` — behind
    `android::attachments`, and driven by `commands::file::stage_attachments`.
    `chatStore`'s `addDroppedPaths` stages **before** any name-based
    classification, since the name is exactly what is wrong until it does.
  - **`lean_web_scrape` downloads documents, not just PDFs.** The tool used to
    special-case exactly one non-HTML type; everything else — a `.docx` linked
    from a page, a raw `.json` API reply, a `.csv` export, a `text/plain`
    README — was refused with `SCRAPE_UNSUPPORTED_CONTENT_TYPE` and a hint that
    the tool reads "HTML pages only", which the model could do nothing with.
    `scrape::download_kind` now classifies the response against an allowlist
    (pdf, docx, xlsx/xlsm/xls, csv, tsv, txt, md, rst, json/jsonl/ndjson, xml,
    yaml/yml, toml, ini, log, srt, vtt), saves it to the shared cache, and
    returns `cached_path` + `file_type` + a hint naming the reader to call
    (`lean_pdf_read_text`, `lean_word_read_text`, `lean_excel_inspect`, or
    `lean_file_read`). Content-type wins over the URL extension when it maps;
    the extension is the fallback for a generic/absent one, which is what the
    old `looks_like_pdf` check did. HTML/XHTML is never a download, whatever
    the URL's extension says. The allowlist stays an allowlist: archives,
    executables and media are still refused, not because the download would
    run anything, but because no bundled reader can open them, so saving one
    would only leave junk on disk.
- **Tool-name fallout**: `lean_excel_*`/`lean_pdf_*` are new names that seed
  adaptive-pathway's Thompson bandit cold; no existing name was renamed
  (adaptive-pathway hashes the literal tool-name string — see
  `plugins/kitty-tools/tests/protocol.rs`).

## Tool-plugin filesystem grants — `KITTY_ALLOWED_DIRS` / `KITTY_ALLOWED_DIRS_FILE` (0.10.1)

A cross-binary contract between the V2 daemon and the two bundled Rust tool
servers. Both sides must be rebuilt together when it changes.

**Why it exists.** `kitty-tools` and `kitty-wasm` enforce their own path
boundary as defense-in-depth behind the daemon's per-session
`check_containment`. That boundary used to be a single "home" resolved from
`KITTY_PLUGIN_HOME` — which is *also* where those servers keep their scratchpad
and extract-once document cache. When `mcp::manager::scoped_env` began giving
each app its own `apps/<id>/plugin-home` (so two apps could not share a
scratchpad), that narrow storage folder silently became the only tree the file
tools would read. The session's chat directory was not inside it, so a model
told by its own system prompt that the user's attached files live there got
`PATH_OUTSIDE_HOME` from every reader and could not open them by any route.

**The split.** `KITTY_PLUGIN_HOME` now governs **storage only**. Authorization
travels separately, in two halves divided by how often they change:

| Variable | Carries | Set by | Read by |
|---|---|---|---|
| `KITTY_ALLOWED_DIRS` | home dir, OS temp dir, daemon data root | `mcp::kitty_grants::static_allowed_dirs`, via `scoped_env` at spawn | `paths::allowed_roots` in both plugins |
| `KITTY_ALLOWED_DIRS_FILE` | path to a JSON array: the session's `attached_paths`, `working_dirs`, `cwd`, `chat_dir` | `mcp::kitty_grants::publish`, rewritten per turn from `allowed_dirs_for_session` | same, mtime+length cached |

The second exists because a stdio MCP server is **one long-lived process shared
by every session**, with its environment fixed at spawn — per-session grants
cannot ride in an env var. Format: a flat JSON array of strings
(`["C:/Users/me/Documents/Kitty/chats/abc", "D:/work"]`); `join_paths`/
`split_paths` conventions apply to `KITTY_ALLOWED_DIRS` (`;` on Windows).

**Failure direction.** Both halves only ever *widen* the plugin-side check. The
daemon has already run the authoritative per-session containment test by the
time a tool executes, so a stale, missing or malformed grants file costs a
spurious rejection — never an unauthorized read. An empty root set rejects
everything, and with neither variable set the allowed set is exactly the home
directory, which is the pre-split behaviour (and what Android, where the daemon
is in-process and the process *is* the app, still uses).

## Context budget schedule (0.10.1)

The compaction schedule is derived from the session's real context window, not
from flat token constants. `agent::compaction` owns both derivations and
`context::builder` uses them, so there is one definition:

- `live_tail_budget` — `min(max_live_tail_tokens, window × LIVE_TAIL_WINDOW_SHARE)`,
  floored at 1024. `LIVE_TAIL_WINDOW_SHARE` is **0.35**.
- `compaction_high_water` — `max(min_compaction_tokens, window × compaction_threshold)`,
  then capped at `window − live_tail_budget − wrapup_reserve`, floored at 2048.

The cap is the load-bearing part. Without it a 36k window waited for 21600
foldable tokens while reserving 24000 for the live tail and 9000 for the reply —
54600 tokens of intent inside a 36000-token window — so the wrap-up valve always
fired first, automatic compaction never ran, and the chat became unusable within
about three turns. `the_context_schedule_is_self_consistent_on_a_small_window`
asserts `high_water + tail + reply <= window` across window sizes; keep it
passing when tuning any of these.

Summarizer prompts are bounded against the same window
(`SUMMARIZER_REPLY_RESERVE`, `SUMMARIZER_PROMPT_MARGIN`) and folded in at most
`MAX_FOLD_PASSES` chunks per pass — on every platform but Windows the summarizer
*is* the session's own chat model, so an unbounded fold prompt was larger than
the window it was sent to relieve.

**These are no longer user-settable.** "Max context tokens", its "Match active
provider" button and "Max live tail tokens" were removed from Settings →
Advanced: they are derived per provider now, and a manual override could
reintroduce the exact bug above. The `TokenManagementConfig` fields remain as
the fallback for a provider that advertises no window.

## Android on BigTiny V2, and the builtin-registration contract (2026-09-09)

Two findings from picking the Android lane back up after two releases. They are
recorded here because both are contracts between files that cannot be checked by
reading either file alone.

### Both targets now run one daemon

`src-tauri`'s `cfg(target_os = "android")` block path-dep'd `plugins/bigtiny_rust`
(V1, 0.1.0) while desktop had moved to `BigTinyV2/daemon` (2.0.0). Android now
links `bigtiny2_daemon` with `litert-embed`. What changed with it:

| | V1 (was) | V2 (now) |
|---|---|---|
| Auth | daemon-wide `BIGTINY_SECRET`, opt-in via `require_secret` | per-app key; **unconditional** — every `/api/*` route but `/api/health` requires one |
| Android's credential | the shared secret | a per-launch `registration_token` (`RunOptions::secret`) exchanged once at `POST /api/apps/register` for a durable app key, kept in the AndroidKeyStore |
| At-rest encryption key | injected by Kitty from the keystore | daemon-owned, `{data_dir}/encryption.key` |
| `RunOptions::recipes_dir` | present | gone (recipes were replaced by specialists) |
| `RunOptions::idle_exit_mins` | — | present; `None` in-process, where the app *is* the daemon's lifetime |

`require_secret` still exists on `RunOptions` but **nothing reads it** — do not
take its presence as evidence that auth is optional.

Two traps that cost time and are not obvious from the code:

- **`async-stream` was declared under `[target.'cfg(windows)'.dependencies]`**
  in the V2 daemon's manifest while `routes/chat.rs` used it unconditionally, so
  the daemon could not compile for *any* non-Windows target. A dependency's
  position in a manifest is invisible until something cross-compiles.
- **V2 renamed the data-dir variable, and only Android notices.** Kitty's
  `daemon_env` sets `BIGTINY_DATA_DIR` (V1's name); V2's `resolve_data_dir`
  reads **`BIGTINYV2_DATA_DIR`** (`discovery::DATA_DIR_ENV`). With it unset the
  function falls back to `dirs_home().join(".bigtiny-v2")`, and bionic reports
  `HOME` as `/` — so the in-process daemon tried to create `/.bigtiny-v2` and
  died with `Read-only file system (os error 30)` *before binding*, surfacing
  only as `BackendDown`. **Desktop cannot reproduce this**: the same function
  short-circuits to `%APPDATA%/BigTinyV2` on Windows before the fallback is
  reached, so the missing variable is invisible there. `bigtiny_embedded::start`
  now sets it explicitly to `config_dir()/bigtiny-v2` — a *sibling* of V1's
  `bigtiny/`, so the frozen V1 crate stays a real rollback path.

  The general lesson, since this will recur: **`daemon_env`'s `BIGTINY_*` names
  are the config-override contract, not the whole environment contract.** V2 has
  its own `BIGTINYV2_*` variables (`DATA_DIR`, `ENCRYPTION_KEY`,
  `IDLE_EXIT_MINS`) that `apply_env_overrides` does not touch, and an embedding
  host has to supply them itself.

- **`cargo ndk -t arm64-v8a --platform 26 check --lib` is the gate**, now also
  run by `.github/workflows/android.yml`. Plain
  `cargo check --target aarch64-linux-android` sets no NDK sysroot or linker and
  is not a substitute. `--platform` must track `minSdk` in
  `gen/android/app/build.gradle.kts` (26).

For the record, the Android dependency delta V1 -> V2 is four **pure-Rust**
crates (`rmcp`'s `server`/`macros` features, `schemars`, `bigtiny2-protocol`,
`async-stream`). Everything with native code — `sqlx`/`libsqlite3-sys`,
`wasmtime` via `kitty-wasm`, `edgefirst-tflite` — is identical in both trees, and
`litert-lm-rust` stays correctly `cfg(windows)`-only. No openssl anywhere; keep
it that way (`reqwest` is `default-features = false` + `rustls-tls` throughout).

### A daemon builtin is only reachable if Kitty registers a row for it

MCP tools reach the model **only** through connected `mcp_servers` rows —
`mcp::manager::connect_all` lists the table and connects what it finds, and the
daemon's `mcp::builtin::BUILTIN_SERVERS` const is referenced nowhere outside its
own test. There is no boot-time auto-provisioning.

`specialists` shipped fully implemented on the daemon side — `call_specialist`
and `list_specialists`, the `builtin::connect` arm, the orchestrator attached,
the roster seeded, session-id injection in `agent::loop_` — and **completely
unreachable**, because `bigtiny::mcp::ensure_builtin_servers` never upserted a
row for it. The mirror image of the earlier `pathway` bug, where Kitty
registered a row the daemon had no arm for.

The contract is now explicit on both sides and tested:

| Side | Const | File |
|---|---|---|
| daemon: what it *can* host | `mcp::builtin::BUILTIN_SERVERS` | `BigTinyV2/daemon/src/mcp/builtin.rs` |
| Kitty: what actually gets a row | `REGISTERED_BUILTINS` | `src-tauri/src/bigtiny/mcp.rs` |

Registering that row exposed two further V2 bugs, **both of which affect
Windows** and neither of which a fresh install can reproduce:

* **`PATCH /api/mcp/servers/{id}` returned 500 `no column found for name:
  app_id`.** `routes/mcp.rs`'s pre-update `SELECT` omitted `app_id` while
  `MCPServerRow` requires it, so `FromRow` failed. Every other query in
  `storage::mcp_servers` selects it; this one was the outlier. Latent because a
  just-created row is never patched — it fires the first time a builtin's
  desired spec changes (a new timeout, an env change), i.e. on upgrade, not on
  install.
* **`mcp.connect_all()` ran before `mcp.attach_orchestrator()`.** The
  `specialists` builtin requires the orchestrator, so it failed at every boot
  with "no orchestrator is attached" and recovered only on a later retry.
  `connect_all` now runs after the attach; nothing else connected there needs
  the agent.

Still open, and **not** caused by this work: the `pathway` builtin can never
connect on V2 (`MCPManager` is constructed with `pathway: None` and the per-app
hand-over its comment describes was never implemented), so the model has lost
`record`/`forget` on both platforms. See `docs/BACKLOG.md`.

`registered_builtins_covers_the_daemons_builtin_servers` fails when the daemon
gains a builtin nothing registers; `every_registered_builtin_is_one_the_daemon_can_host`
fails in the other direction; `upsert_builtin` `debug_assert!`s membership. **Change
the two lists together.**

Note also that `specialists` is the one builtin whose MCP timeout must not be
left at the daemon's 30s default: `call_specialist` blocks the parent's tool
call for the whole delegate run, bounded at `SpecialistSettings::timeout_secs`
(300s default). The row is registered at `timeout_secs + 30` so the orchestrator,
not the MCP layer, is what decides a delegate has run too long.

## Artifacts pane scope — grants, not "every path a tool touched" (0.10.2)

The heuristic above decides whether a tool call *produced a file*. It never
decided whether that file is one the **user** would recognize, and the two are
not the same question. Both plugins keep working storage under
`~/.cache/lean-goose-mcp`: `kitty-tools` writes every paged read through its
extract-once document cache (`plugins/kitty-tools/src/doc_store.rs`), and
`kitty-web`'s scrape/download path files its copy there too. Those writes carry
a `path` ending in `.json`/`.md`/`.txt`, so they matched `ARTIFACT_EXT_RE` and
landed in the pane — reading one PDF filed a cache artifact the user has no
reason to know exists. Anything a tool staged under the OS temp dir did the
same.

`isArtifactInScope` (`src/stores/chat/messageUtils.ts`) now gates every
tool-derived artifact on the session's **grant set** rather than on a
blocklist of cache directories:

| Source | In the pane? |
|---|---|
| `source: 'user'` (attached by drag/paste) | always — never routed through the check |
| `source: 'disk'` (the `cwd` scan) | always — `refreshArtifactsFromDisk` only lists `cwd` |
| `source: 'tool'` | only inside `SessionAllowedDirs` |

`SessionAllowedDirs` (`GET /api/chat/{id}/allowed_dirs`) is already cached in
`chatStore` for the approval handler, and is deliberately *not* the full sandbox
set — it excludes the daemon's data root and its plugins' cache directories,
which is exactly the distinction the pane needs. Its members are `chat_dir`,
`cwd`, every `working_dirs` entry set during the session, and every
`attached_paths` entry. So "chat home, selected working directory, attachments"
is not restated here; it is read off the same grants that make those places
reachable by tools in the first place, and the two cannot drift.

Two deliberate choices:

- **Blocklisting the cache paths was rejected.** A blocklist has to be kept
  current with every plugin's storage decisions, and it silently fails open —
  a new cache directory shows up in the pane until someone notices. The grant
  set fails closed and is maintained for other reasons anyway.
- **An empty scope keeps the artifact.** That state means "the grants have not
  loaded yet", not "this session may touch nothing" — the pre-load fallback is
  `chatDir`/`cwd`, and swallowing a real output is the worse failure. Same
  reasoning as `decideChatApproval`'s `bases.length > 0` guard.

Attachment grants are per *file*, not per folder: `pathWithinDir(file, file)` is
true by the `resolved === b` arm, so attaching `~/Downloads/a.pdf` does not make
the rest of `~/Downloads` visible in the pane.

## Specialist tool calls and the HITL gate (0.10.3)

**Every specialist that used a tool failed, on every run, on both platforms.**
Registering the `specialists` MCP server in 0.10.2 made `call_specialist`
reachable for the first time, which is what exposed this: the delegate would
spend its whole run being refused and then answer anyway.

The chain. A delegate is spawned with `hitl_policy: "auto_reject"`
(`agent/orchestrator.rs`) because there is no user attached to answer an
approval prompt and `always_ask`'s wait is bounded at an hour *per tool call*.
The shipped `config.hitl.default_policy` is `always_ask` and
`always_allow_patterns` is empty, so a tool with no stored rule resolves to
`needs_approval` — and in an unattended run that became an immediate refusal.
The researcher's three tools (`lean_web_search`, `lean_web_search_read_chunk`,
`lean_web_scrape`) have no rule, so it could never search.

Observed on a Pixel 10 Pro, 2026-09-10: five turns, every `lean_web_search`
answered with "requires human approval, which is unavailable in an unattended
run", after which the model **fabricated a bibliography from memory** —
plausible authors, venues and dates for a literature search it had been unable
to perform. The `COMMON` preamble tells specialists to report refusals in
`refusals` rather than present a partial result as a complete one; a small
model handed a blocked tool mid-task does not reliably comply. That is why this
is a correctness bug and not a papercut.

**Why Android showed it and Windows had not.** Nothing platform-specific — the
Android install is a fresh V2 database (D26c) with zero `hitl_rules`, so
*everything* is undecided. The Windows database had accumulated three
`always_allow` rows from earlier sessions (`lean_analyze_workspace`,
`lean_pdf_read_outline`, `lean_pdf_read_text`), which would have let `locator`
and parts of `summarizer`/`extractor` through while `researcher` failed there
too. The bug is identical on both; the symptom depends on which rules a user
happens to have accrued.

### The fix, and what it deliberately does not cover

`HITLDecision` gains `from_default_policy`, true only when no auto-reject
pattern, no always-allow pattern and no stored rule matched — the difference
between *the user decided this needs a human* and *nobody has decided anything
about this tool yet*. `agent/loop_`'s unattended branch proceeds on the second
case when the tool is on the run's `tool_allow` list, and refuses as before on
everything else:

| Classification | Unattended run |
|---|---|
| auto-reject pattern | rejected (unchanged) |
| stored `reject` rule | rejected (unchanged) |
| stored rule too damaged to apply | refused — fails closed (unchanged) |
| containment escalation (path outside `allowed_dirs`) | refused (unchanged) |
| default policy, tool on `tool_allow` | **proceeds** |
| default policy, no `tool_allow` on the session | refused (unchanged) |

`tool_allow` is not a hint being promoted to an authorization here. It is a
decision the user made in writing when they wrote the specialist — the same
decision the approval prompt would have asked for — and it was already enforced
twice before this point (the advertised tool set is narrowed to it in
`run_inner`; `execute_one_tool_call` re-checks it at dispatch). The delegate
also inherits the parent's filesystem grants and nothing more, so it can reach
nothing its parent could not, and `check_containment` still runs.

`hitl_auto_reject` is set by exactly one caller — the orchestrator — so this
touches delegate runs only. A session with no `tool_allow` at all is still
refused: absent means unrestricted, which is the opposite of a
pre-authorization.

All five builtin specialists are read-only or sandboxed (web reads, document
readers, `wasm_python_run`); none names `shell` or a write tool. A
*user-authored* specialist that lists one will now run it unattended, which is
what listing it means.

## Saving files out of the Android app, and opening links (0.10.4)

Three reported faults, one root cause: **Kitty had no working write path off
Android.** Everything the app can write with `std::fs` is inside its private
data directory, which no other app — including the system Files app — can see
into. Anything leaving Kitty has to go through a `content://` URI the user
granted, and `std::fs` cannot open one: there is no file there, only a provider
that will hand out a stream through the ContentResolver.

Each save path met that mismatch differently, which is why it read as three
unrelated bugs:

| Path | URI it got | What went wrong |
|---|---|---|
| `exportSession` (single chat) | document, from `ACTION_CREATE_DOCUMENT` | `std::fs::write` on a URI → "directory isn't writeable", including for Drive |
| `exportSelected` (bulk, Saved Chats) | **tree**, from `ACTION_OPEN_DOCUMENT_TREE` | built `` `${dir}/${base}.jsonl` `` — a tree URI has no join; the result is not a valid URI of either kind |
| `download_file` (artifacts) | document | opened via `tauri-plugin-fs` with mode `"wt"`; providers accept the `t` flag and commit nothing → **zero-byte file, no error** |

`android::documents` replaces all three with one Kotlin entry point
(`KittyPlugin.writeDocument`). It takes either URI kind — creating a document
inside a tree via `DocumentsContract.createDocument` when given one — opens
`"w"` rather than `"wt"` (truncation is meaningless on a document
`ACTION_CREATE_DOCUMENT` has just created), and **returns the byte count the
provider actually accepted**. Callers verify against what they sent, so a short
write is an error the user sees rather than an empty file they find later. A
new `write_file_in_dir` command exists because "directory plus name" cannot be
a path join on Android; desktop keeps the join, so callers stay
platform-agnostic.

Two smaller decisions inside that:

- **`.jsonl` is declared `text/plain`, not `application/json`.** A provider may
  correct a document's extension to match its MIME type, and JSON Lines is not
  JSON — declaring it as such would silently rename every chat export to
  `.json`.
- **`tauri-plugin-fs` is no longer registered or depended on.** Its mode
  handling was the zero-byte bug, and `download_file` was its only consumer.

### Links in chat did nothing on Android

Separate bug, same release. `tauri-plugin-opener`'s **mobile `open_path` is
broken**: it sends the target as a bare JSON string
(`run_mobile_plugin("open", path.into())`), while its own Kotlin side does
`invoke.parseArgs(OpenArgs::class.java)` and expects `{url, with}`. Every call
rejected before reaching an Intent. Its `open_url` builds that object
correctly.

Markdown links now go through a new `open_url` command, which is the right
entry point for a URL on every platform anyway. It refuses anything that is not
`http(s)`: a markdown link is attacker-influenced text — a model can be talked
into emitting one — and `open_url` will otherwise hand any scheme to the
system, including `file://` and `intent://`.

`open_path` keeps its filesystem callers (artifacts Open / Show in Folder),
which are desktop-only and were never affected.

### Where the tests are

`base64_encode` lives in `util.rs`, not beside its only caller in
`android::documents`. That module is `cfg(target_os = "android")`, so tests
inside it never run on the desktop or in CI — and a wrong base64 pad is a
corrupted export that still reports success. The RFC 4648 vectors are pinned
where they actually execute.
