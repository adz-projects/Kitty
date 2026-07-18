# Reorganization Plan — Goose Overlay (Kitty)

> Based on: `PROJECT_INDEX.md`, `INVESTIGATE.md` (sections 1–2), and exploration of two external projects (`replacement_mcp`, `Adaptive Pathway Learning`).
> Goal: A codebase future developers can read, modify, and extend without needing tribal knowledge.

---

## 0. Current State Summary

### Kitty (the host app)
- **42 Rust files** (~8.5k LOC), **123 TypeScript/TSX files** (~7k LOC)
- **5 cyclic deps** (3 actionable, 2 worth monitoring)
- **8 files >500 LOC** — worst: `chatStore.ts` (2313), `providers.rs` (799), `session.rs` (730), `lifecycle/mod.rs` (725)
- **50+ `Mutex::lock().unwrap()`** in async contexts (with `panic=abort` in release)
- **22 `reqwest::Client::builder()` sites** — each builds TLS state independently
- **Large undocumented surface**: Adaptive Pathway, Recipes, ScheduledTasks, folder bookmarks, log capture, provider_trust UI — none of these appear in `CLAUDE.md`'s 11-phase plan
- **0 custom traits** — all concrete types, heavily monomorphized

### `replacement_mcp` (to internalize)
- **1 Python file** (`lean_mcp.py`, 889 LOC) — MCP server with 10 context-optimized tools
- **1 YAML config** (`tool_prompts.yaml`)
- **~1,928 total LOC** (mostly documentation)
- **0 tests, 0 CI, 0 package config** — PEP 723 inline deps, ad-hoc
- **Communication**: stdio MCP (Goose client), outbound HTTP (web scrape/search)
- **Purpose**: Replace Goose's built-in `developer` + `computer_controller` extensions with leaner tools for local 35B models

### `Adaptive Pathway Learning` (to internalize)
- **~4,600 LOC Python source** (14 subdirectories), **~4,100 LOC tests** (19 test files)
- **Two integration surfaces**: MCP server (stdio) + HTTP sidecar (FastAPI, port 8700)
- **SQLite persistence**, **Ollama embeddings**, **7-model bandit ensemble**
- **Already partially wired into Kitty**: Rust `adaptive_pathway/` module (lifecycle management), `commands/adaptive_pathway.rs` (17 Tauri commands), `lifecycle/adaptive_pathway_proc.rs` (sidecar spawn), TS `adaptivePathwayStore.ts`, UI components (`AdaptivePathway.tsx`, `GraphHealth.tsx`, `DomainProfiles.tsx`, `SchismResolutionModal.tsx`, `NudgeConsentPrompt.tsx`)
- **Significant duplication**: Kitty's Rust `adaptive_pathway/mod.rs` duplicates the Python sidecar's concept of edges/state/metrics — the Python is the source of truth, the Rust client is a thin adapter
- **Missing from spec**: The entire AP subsystem is undocumented in `CLAUDE.md` despite being the most architecturally complex part of the app

---

## 1. Phase 1 — Break Dependency Cycles (Rust)

| Step | Cycle | Move | From | To | Risk |
|------|-------|------|------|----|------|
| 1.1 | Cycle 1 | `ManagedProcess` struct + `StackStatus` enum | `lifecycle/mod.rs` | `state.rs` (remove `state → lifecycle` edge) | Low — types are small, no behavior depends on lifecycle module |
| 1.2 | Cycle 2 | Scheduled-task execution logic | `lifecycle/mod.rs` | New `lifecycle/scheduler.rs` (takes `AppHandle`, calls `commands::session` explicitly — no reverse dependency) | Low — pure extraction |
| 1.3 | Cycle 3 | `requires_local_ollama()` function | `config/providers.rs` | `lifecycle/ollama_proc.rs` (already imports `ollama_proc` for `probe_version`; now `ollama_proc` has zero deps on siblings) | Low — pure function move |
| 1.4 | Cycle 5 | Shared ACP response types | `goosed/api.rs` + `goosed/stream.rs` | New `goosed/types.rs` (used by both, depends on neither) | Low — mechanical extraction |

**After Phase 1**: All 3 actionable cycles resolved. Cycle 4 (notifications chain) remains but each hop is shallow and acceptable.

---

## 2. Phase 2 — Split Overlarge Files (Rust)

### 2.1 `config/providers.rs` (799 LOC → 4 files)

```
src-tauri/src/config/
  mod.rs                  # re-exports + Config struct (keep ~150 LOC)
  providers/
    mod.rs                # re-exports submodules, pub ProviderProfile struct
    network.rs            # network_tier computation, requires_local_ollama (moved from lifecycle)
    keyring.rs            # get_secret, set_secret, delete_secret
    connection.rs         # test_connection (async HTTP probe per provider type)
    env.rs                # goosed_env() — builds env var map for goose serve
```

Rationale: `providers.rs` currently mixes a data model (`ProviderProfile`), trust-tier logic (`network_tier`), secret I/O (`keyring`), HTTP probing (`test_connection`), and environment assembly (`goosed_env`). Each is a separable concern.

### 2.2 `commands/session.rs` (730 LOC → 3 files)

```
src-tauri/src/commands/
  session/
    mod.rs                # re-exports, shared helpers (resolve_cwd, new_chat_folder)
    crud.rs               # new_session, load_session, fork_session, delete_session, clear_all_sessions
    prompt.rs             # send_prompt, cancel_prompt, respond_permission
    config.rs             # set_mode, set_thinking_effort, rebind_session_provider
```

Rationale: 13 `pub async fn` in one file covering three distinct concern areas. Separating makes `prompt.rs` a focal point for streaming/retry logic.

### 2.3 `lifecycle/mod.rs` (725 LOC → 4 files)

```
src-tauri/src/lifecycle/
  mod.rs                  # re-exports, start_stack, shutdown (orchestration only)
  scheduler.rs            # spawn_scheduler_loop, fire_scheduled_task, advance_scheduled_task
  embedding.rs            # ensure_embedding_model, set_embedding_status
  health.rs               # compute_status, spawn_health_loop, spawn_adaptive_pathway_health_loop
  adaptive_pathway_proc.rs (keep, already exists)
  goosed.rs               (keep, already exists)
  ollama_proc.rs          (keep, already exists)
  conflict.rs             (keep, already exists)
```

---

## 3. Phase 3 — Split Overlarge Files (TypeScript)

### 3.1 `stores/chatStore.ts` (2313 LOC → 5 stores)

```
src/stores/
  chatStore.ts            # core: session state, active message list, send/stop/clear (keep ~400 LOC)
  streamStore.ts          # bufferDelta/flushDeltas, delta accumulation, rAF scheduling
  artifactStore.ts        # tool-call scanning, artifact derivation from file-writing tools
  approvalStore.ts        # pending approvals, respondPermission, mode auto-approval logic
  errorStore.ts           # error humanization, loop detection, tool-loop guard
```

Rationale: `chatStore.ts` currently mixes streaming state, artifact detection (scanning tool calls for file writes), error humanization (pattern-matching error messages), loop detection (repetition guard), and mode inference (auto/manual/approve). Splitting preserves the single `invoke()` chokepoint in `ipc.ts` while making each concern independently testable. Each new store uses Zustand's `subscribe` to cross-communicate (pattern already established in the codebase).

Migration path: 1) Extract `streamStore.ts` first (no dependencies on other stores). 2) Extract `artifactStore.ts` and `approvalStore.ts` (both depend on `chatStore` for session id). 3) Extract `errorStore.ts` last (depends on message content). Each step keeps the old `chatStore.ts` re-exporting until tests pass.

### 3.2 `components/settings/Recipes.tsx` (714 LOC)

Split into:
```
src/components/settings/recipes/
  RecipeList.tsx          # table + search + delete
  RecipeEditor.tsx        # create/edit form (YAML source + field editors)
  RecipeImportExport.tsx  # import/export dialogs
  RecipeExtensions.tsx    # extension binding UI
```

### 3.3 `components/settings/Providers.tsx` (643 LOC)

Split into:
```
src/components/settings/providers/
  ProviderList.tsx        # table of profiles + activate/test/delete actions
  ProviderForm.tsx        # add/edit form (per-provider-type fields)
  ProviderKeyDialog.tsx   # key entry + privacy-warning dialog
  ProviderTierBadge.tsx   # network-tier badge component
```

---

## 4. Phase 4 — Async Safety & Blocking Ops

| Step | Issue | Fix | LOC Changed |
|------|-------|-----|-------------|
| 4.1 | 50× `Mutex::lock().unwrap()` in async | Replace `config`, `goosed`, `ollama` Mutex fields with `RwLock`; replace `stack_status` with `AtomicU8` | ~80 lines across state.rs + 12 command files |
| 4.2 | `std::fs::create_dir_all` in `new_session`, `delete_session` | Wrap in `tokio::task::spawn_blocking` | ~6 lines in `commands/session.rs` |
| 4.3 | `std::fs::remove_dir_all` in `delete_session`, `clear_all_sessions` | Wrap in `tokio::task::spawn_blocking` | ~4 lines |
| 4.4 | 22× `reqwest::Client::builder()` | Store one shared `Client` in `AppState` (built once at startup, `reqwest::Client` is cheap to clone as it's Arc-backed internally) | ~30 lines across state.rs + all callers |
| 4.5 | `keyring::get_password` in async provider path | Cache secrets in `AppState` (loaded at startup, evicted on provider edit) | ~40 lines, new `secrets: RwLock<HashMap<String, String>>` field |
| 4.6 | `mpsc::unbounded_channel` in ACP writer | Switch to `mpsc::channel(1024)` with `send().await` backpressure | ~5 lines in `goosed/api.rs` |

---

## 5. Phase 5 — Frontend Responsiveness

These are the high-ROI fixes from the responsiveness audit:

| Step | Issue | Fix | Files |
|------|-------|-----|-------|
| 5.1 | Branch/Regenerate/Export buttons with no `disabled` state | Add `busy` ref + `disabled` prop to each button | `MessageItem.tsx` (~15 lines) |
| 5.2 | Approve/Deny buttons with no `disabled` state | Add local `submitting` state | `ApprovalPrompt.tsx` (~8 lines) |
| 5.3 | Session resume buttons with no loading feedback | Add `loading` state, close popover only after load | `RecentSessions.tsx`, `SessionList.tsx` (~10 lines) |
| 5.4 | Forced layout on every keystroke in composer | Replace `scrollHeight` read with `field-sizing: content` CSS + rAF | `Composer.tsx` (~5 lines), `base.css` (1 line) |
| 5.5 | No debounce on session search | Add 150ms debounce to `setQuery` | `SessionList.tsx` (~3 lines) |
| 5.6 | `ThinkingBox` never auto-expands during reasoning | Auto-open when streaming, close on completion | `ThinkingBox.tsx` (~6 lines) |

---

## 6. Phase 6 — Internalize `replacement_mcp`

### 6.1 Current State

`replacement_mcp` is a standalone Python project (889 LOC) with no host integration. It's designed to be registered as a Goose MCP extension by editing Goose's config YAML manually. Kitty does not manage it.

### 6.2 Integration Plan

**Decision: Embed as an optional managed-process MCP extension, spawned by the existing `lifecycle` machinery.**

```
src-tauri/src/
  lifecycle/
    mcp_extensions/
      mod.rs              # spawn/monitor managed MCP extensions (ReplacementMCP process)
      replacement_mcp.rs  # Spawns `uv run lean_mcp.py`, tracks process, probes health
```

Steps:
1. Vendor `lean_mcp.py` into `src-tauri/python/replacement_mcp/lean_mcp.py`
2. Vendor `tool_prompts.yaml` into `src-tauri/python/replacement_mcp/tool_prompts.yaml`
3. Add `replacement_mcp_enabled: bool` to `Config` (default: false)
4. Add `ManagedProcess` field to `AppState` for the replacement MCP child
5. Wire spawn/stop into `lifecycle/mod.rs::start_stack` (gated by config toggle)
6. Add `#[tauri::command] restart_replacement_mcp`, `get_replacement_mcp_status`
7. Register as a Goose MCP extension by writing to Goose's config on startup (via `goose_config.rs`)
8. Add `mcp_extensions` health probe to `compute_status` (separate from stack health, non-blocking)
9. Add Settings UI toggle in Extensions section

### 6.3 Directory Layout After Integration

```
src-tauri/
  python/
    replacement_mcp/
      __main__.py           # entry point (tiny, just calls lean_mcp.py's main)
      lean_mcp.py           # vendored, 889 LOC
      tool_prompts.yaml     # vendored config
    tests/                  # smoke tests for the vendored MCP
      test_lean_mcp.py
```

---

## 7. Phase 7 — Internalize Adaptive Pathway Learning

### 7.1 Current State (messy)

- **Python sidecar**: `Adaptive Pathway Learning/` — 4,600 LOC, 19 test files, SQLite, Ollama, FastAPI
- **Rust client**: `adaptive_pathway/mod.rs` — 15 async HTTP client functions duplicating the sidecar's API surface
- **Rust lifecycle**: `lifecycle/adaptive_pathway_proc.rs` — spawns/monitors the sidecar
- **Rust commands**: `commands/adaptive_pathway.rs` — 17 Tauri commands
- **TS UI**: `adaptivePathwayStore.ts`, `AdaptivePathway.tsx`, `GraphHealth.tsx`, `DomainProfiles.tsx`, `SchismResolutionModal.tsx`, `NudgeConsentPrompt.tsx`
- **Unknown**: The Python sidecar has a FastAPI HTTP server (port 8700) AND an MCP server (stdio). Kitty's Rust client communicates via HTTP (detected from `adaptive_pathway/mod.rs`'s HTTP calls). The MCP server is unused by Kitty.

### 7.2 Integration Plan

**Decision: Keep the Python sidecar as a managed child process (current architecture), but formalize the contract and reduce duplication.**

```
src-tauri/
  adaptive_pathway/         # Rust HTTP client (keep, but slim)
    mod.rs                  # Keep: 15 async HTTP client functions (thin wrappers)
  lifecycle/
    adaptive_pathway_proc.rs # Keep: spawn/monitor/stop (already correct)
  commands/
    adaptive_pathway.rs     # Keep: 17 Tauri commands (already correct — thin wrappers)
```

Steps:
1. **Vendor the Python sidecar**: Copy `src/adaptive_pathway/` → `src-tauri/python/adaptive_pathway/` (keep the HTTP sidecar entry point, discard the MCP server — Kitty uses HTTP)
2. **Consolidate startup env vars**: Ensure `ADAPTIVE_PATHWAY_DB`, `AP_EMBED_OLLAMA_URL`, `AP_EMBED_OLLAMA_MODEL` are all passed through `lifecycle/adaptive_pathway_proc.rs` via env (currently only partial)
3. **Remove Rust-side duplication**: The `adaptive_pathway/mod.rs` HTTP client duplicates the JSON request/response shapes that the Python sidecar's FastAPI endpoints define. Add a `docs/adaptive-pathway-api.md` documenting the HTTP contract — the Rust and Python sides both reference it. Remove any dead endpoint wrappers.
4. **Keep the Python tests**: Vendor `tests/` alongside the source. Add a `[target.'cfg(windows)'.dependencies]` `pytest` runner or separate integration test step.
5. **Document in `CLAUDE.md`**: Add the Adaptive Pathway section to the phased plan (it's currently completely absent from the spec — the most important documentation gap).

### 7.3 Directory Layout After Integration

```
src-tauri/
  python/
    adaptive_pathway/
      __main__.py              # entry: uv run python -m adaptive_pathway.integrations.sidecar
      adaptive_pathway/
        engine.py              # core orchestrator
        features.py            # feature hashing
        health.py              # health checks
        embeddings.py          # Ollama embedding client
        types.py               # dataclasses
        config/
          defaults.yaml        # config defaults
        decision/              # bandit ensemble (7 models)
        learning/              # curiosity, preferences, TTL
        storage/               # SQLite, vector index, tiered cache
        discovery/             # primitives, domains, centroids
        integrations/
          sidecar/             # FastAPI server (Port 8700, HTTP)
    tests/
      test_engine.py           # 1047 LOC — keep as-is
      test_mcp_server.py       # 399 LOC — DISCARD (Kitty uses HTTP, not MCP)
      test_sidecar.py          # 210 LOC — keep
      test_ensemble.py         # 310 LOC — keep
      ...                      # keep remaining 16 test files
```

---

## 8. Phase 8 — Documentation & Naming

| Step | What | Why |
|------|------|-----|
| 8.1 | Update `CLAUDE.md` | Add Adaptive Pathway, Recipes, ScheduledTasks, folder bookmarks, log capture, replacement MCP sections. Without this, every new developer discovers undocumented subsystems by reading code. |
| 8.2 | Create `docs/ARCHITECTURE.md` | One-page module map showing dependency direction (the flat inventory in `PROJECT_INDEX.md` has no edges — future devs need to see the arrows) |
| 8.3 | Create `docs/ADAPTIVE_PATHWAY.md` | HTTP contract between Rust client and Python sidecar: all endpoints, request/response shapes, status codes |
| 8.4 | Create `docs/MCP_EXTENSIONS.md` | How managed MCP extensions work (spawn, register, probe, restart) — covers both Adaptive Pathway and replacement MCP |
| 8.5 | Create `docs/VERSIONS.md` | Record pinned goosed/Goose version + Ollama version + adaptive-pathway sidecar version (UPDATE: this already exists but is empty — populate it) |
| 8.6 | Rename `commands/adaptive_pathway.rs` → `commands/ap.rs` | 17 Tauri commands, all prefixed `adaptive_pathway_*` — module name can be shorter without losing meaning |
| 8.7 | Standardize Rust module exports | Every `mod.rs` should either re-export submodules (`pub mod foo; pub use foo::*;`) or define types inline, not both. Current state: mixed. |

---

## 9. Execution Order & Dependencies

```
Phase 1 (break cycles) — no code changes needed in other phases
  ↓
Phase 4.1 (RwLock for Mutex) — touches state.rs, must precede Phase 2.2/2.3
  ↓
Phase 2 (split Rust files) — mechanical extraction, low risk
  ↓
Phase 7 (internalize AP) — depends on Phase 2.3 (lifecycle is split)
  ↓
Phase 6 (internalize replacement MCP) — depends on Phase 7 (same pattern)
  ↓
Phase 3 (split TS files) — independent of Rust changes
  ↓
Phase 5 (frontend responsiveness) — independent, can start early
  ↓
Phase 8 (documentation) — lasts through all phases, do last pass after everything else
```

Phases 5 and 3 can run in parallel with Phases 1–2 (different languages, different engineers).

---

## 10. Summary of File Count Changes

| | Before | After | Delta |
|---|--------|-------|-------|
| Rust files | 42 | ~55 | +13 (split large files + internalized Python) |
| TypeScript files | 123 | ~135 | +12 (split large files) |
| Python files (vendored) | 0 | ~30 | +30 (AP sidecar + replacement MCP) |
| Files >500 LOC | 8 | ~2 | -6 |
| Cyclic deps | 5 | 2 | -3 |
| `Mutex::lock().unwrap()` in async | 50+ | ~5 | -45+ |
| `reqwest::Client::builder()` sites | 22 | 1 | -21 |
| Test files | 18 TS + 0 Rust unit | 18 TS + 19 Python | +19 (vendored AP tests) |
