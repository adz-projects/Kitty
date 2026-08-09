# Kitty — Android & Local-Inference Plan

One Tauri v2 codebase for **Windows desktop** and **Android**. The BigTiny daemon
(`plugins/bigtiny_rust/`) hosts an **in-process llama.cpp** engine and serves chat,
compaction summarization, and desktop-only embeddings. **Ollama is removed
everywhere** — no `ollama serve`, no pull API, no HKCU env vars, no spawned
inference binary.

Status: **PLAN — approved. Not yet implemented. Execution order in §10.**

- Scope-at-a-glance: desktop keeps full local chat + AP + stdio MCP; Android is
  **cloud-chat only** with a packed single local summarizer model, **and still
  runs AP** — in-process inside the daemon, hash-space embeddings only.
- This document is the spec an LLM/coder executes against. Each phase lists the
  concrete targets, files, config shape, and acceptance criteria.
- `docs/ANDROID-PLAN.md` is the companion **execution** plan (toolchain
  bootstrap, per-gate commands, verified environment facts). This file is the
  *spec*; that file is the *order of operations*. Keep them in sync.

### Finalization pass (2026-08-08)

This spec was re-derived against the current codebase before execution. What
changed:

- **The in-process MCP mechanism already exists** and already hosts
  `kitty-tools`, `kitty-web`, `kitty-wasm` **and** `pathway` — not just AP.
  D8/§2.1/§2.3 previously read as if this were open design work. Corrected in
  §2.3, with the real files cited. Android's remaining work here is
  cross-compiling those path-deps plus a per-platform registration flip.
- **Four decisions added (D22–D25)** covering gaps the spec never addressed:
  target/SDK levels, desktop-only `src-tauri` subsystems that must be
  `cfg`-gated (one of which, `winreg`, is a literal first-build breaker
  today), the `keyring` Android backend's JNI-init requirement, and the
  Android loopback-security posture.
- **New §2.4** documents the Android tool surface (what already degrades
  gracefully, what needs scoping).
- **Phase 1, 2, 7, 8 acceptance criteria extended**; §11 reconciled with D1
  and given the two new risk classes.

D1–D21 are unchanged and un-renumbered — existing references (including in
`ANDROID-PLAN.md`) stay valid.

### Execution status (2026-08-08)

- **Phase 0 — done.** Baseline committed and green.
- **Phase 1a — done.** Toolchain installed and verified (D22 records the pinned
  versions). `src-tauri` cross-compiles clean for `aarch64-linux-android`;
  §2.5 is the record of what that took, and it was **more than this doc
  originally listed** — `screenshot.rs` and `config/env_helper.rs` were both
  unlisted hard breakers, and the overlay builder needed the *functions*
  gated, not just their call site.
- **Phase 1 (1-compile) — PASSED 2026-08-08.** The make-or-break gate is
  cleared. `llama-cpp-2` 0.1.154 pinned behind an opt-in `local-engine`
  feature; loads `lfm2` and generates coherent text on Windows; and
  `cargo ndk -t arm64-v8a --platform 26 build --lib --features local-engine`
  succeeds in 3m22s, emitting genuine `EM_AARCH64` objects
  (`libggml{,-cpu,-base}.a`). **`wasmtime` cross-compiles too** (122 MB rlib,
  with `kitty-wasm` at 83 MB) — the "drop kitty-wasm on Android" contingency
  is **not** needed. All four linked path-deps build for the target.
  §11 records the four toolchain traps this surfaced.
- **Still open from Phase 1:**
  - **1-device** — no device attached (`adb devices` empty). NDK linkage
    problems only surface at `dlopen` time, so this remains a real gap.
  - **`nm -u` / "no unresolved externals" cannot yet be checked.**
    `bigtiny_rust` builds as an rlib, so there is no link step. A truthful
    check needs the daemon linked into a cdylib, which is Phase 7's in-process
    work. Not counted as met.
  - **D20 backend selection unvalidated** — Windows is CPU-only for now
    (`cuda`/`vulkan` are cargo features; toolkit deferred), so `-1` = "all
    layers" across backends is untested.

---

## 1. Decisions (definitive register)

| # | Decision | Where it lands |
|---|----------|----------------|
| D1 | **In-process llama.cpp, day 1, both OSes.** `bigtiny_rust` links **`llama-cpp-2` (`utilityai/llama-cpp-rs`), pinned at 0.1.154** — chosen in Phase 1 and validated end-to-end. The `llama_cpp` crate this doc used to name as a placeholder is **abandoned** (last release 2024-04-29). The `LocalEngine` boundary (§3.1) keeps the binding swappable. Chat, compaction, and desktop embeddings all go through the daemon; Kitty manages no inference process of its own (see §10 Phase 2 for what "no Ollama" does and doesn't mean). | §2, §4, §11 |
| D2 | **Model is pinned per chat.** Locked in at chat creation; changes only on **new chats**. No mid-stream swap, ever. | §4.1 |
| D3 | **Scheduled tasks default to the summarizer model**; per-task override in Settings. | §7 |
| D4 | **REVISED 2026-08-09 — semantic embeddings on BOTH platforms, one shared model.** `Qwen3-Embedding-0.6B` (q4_k_m GGUF, 376 MB, 1024-dim) runs through the same in-process engine on Windows *and* Android. AP calls the new `POST /api/embeddings`; `AP_EMBED_OLLAMA_URL` re-points to the daemon port. The hash-space vectorizer (`HASH_EMBED_MODEL`) is demoted to a **fallback** — used before the model is downloaded, or if it fails to load — not the Android norm. *Supersedes the original "desktop only / Android is hash-space" decision; §11's recall-quality-gap risk is retired with it.* | §3.1, §9 |
| D5 | **In-app model downloads** from **HuggingFace** and the **Ollama registry** (manifest → blob → reassembled GGUF). Range+resume+sha256. | §5 |
| D6 | **Exposed llama.cpp engine knobs** + **Quick Presets** (Precise/Balanced/Creative). | §6 |
| D7 | **No proactive disk quota.** Only a low-free-space warning; manual delete. **Hard refusal when `free_space < model_size × 1.5`**. | §5 |
| D8 | Adaptive-Pathway runs **on both OSes, in-process inside the BigTiny daemon** (linked crate, MCP `in_process` "pathway" server — never a separate process). Windows: daemon is a Kitty-managed sidecar; Android: daemon is in-process. Android runs the *full* AP engine (recall/surface/consolidate) **with the same semantic embeddings as desktop** (D4, revised — no longer hash-space); MCP tool servers: **stdio sidecar on Windows**, **`in_process` on Android** — the `in_process` transport and all four servers already exist (§2.3); Android needs cross-compilation and a registration flip, not new machinery. | §2, §2.3 |
| D9 | Frontend = 2 windows (`overlay` + `hub`); settings/wizard fold in-page. Android = same hub, mobile-rendered. | §8 |
| D10 | **Model card** (size/RAM/VRAM/backend/one-tap new-chat default) + **"Recommended for this device" badge** computed from the fit function. | §6.3 |
| D11 | Load-time param changes schedule an **automatic engine restart**: after the current LLM generation completes, or immediately if nothing is generating. Sessions show a non-blocking "restarting" chip. | §6.4 |
| D12 | **Summarizer fallback = the session's chosen model** (may be a cloud provider). Chain: `Local → session model → explicit error`. **No Gemini Nano.** | §4.3 |
| D13 | **Model health always visible** in the model picker (green/red dot). | §6.3 |
| D14 | **Wizard is platform-agnostic**; only Android difference = permissions step. | §8.3 |
| D15 | **Shared design tokens** consumed by both render paths; **dark/light follows the OS** (manual override allowed). | §8.4 |
| D16 | **Safe-area tokens** (`--safe-*`): real `env()` on Android, `0` on desktop. | §8.4 |
| D17 | **Font parity**: Android accessibility font scale and Windows zoom both drive one shared `--font-scale`/rem ramp; **full px→rem sweep of `base.css`**. | §8.4 |
| D18 | **Android = cloud-chat only.** No local *chat*, no local model picker, no per-chat local choice. **Amended by D4's revision:** the local engine on Android now runs **two** models, not one — the summarizer *and* the embedder (~1.1 GB of GGUFs between them). Still no chat slot. | §2.2, §4.1, §4.2 |
| D19 | **`flash_attn` auto-detected** at engine init; never a user toggle (read-only card diagnostic `"off"`/"on (<backend>)"). | §3.3 |
| D20 | **`auto`/`-1` select backend**: `select_backend()` returns `Cuda|Vulkan|Cpu`; fit/badge/VRAM math uses the *selected* backend's VRAM bank. | §3.3 |
| D21 | **Windows multi-window is preserved**: two (or more) hub windows may be open at once, each with its **own independent session and its own pinned model**. Android stays single-window. | §8.1, §4.2 |
| D22 | **`aarch64-linux-android` is the only shipped v1 ABI** (no armeabi-v7a, no x86_64 — emulator testing is a dev convenience, not a release target). **minSdk 26, targetSdk 34.** **NDK pinned: `27.2.12479018` (r27c)**, SDK `platforms/android-34` + `build-tools/34.0.0`, JDK 17, installed and verified 2026-08-08. | §10 P1; `ANDROID-PLAN.md` P1a |
| D23 | **Desktop-only `src-tauri` subsystems are `cfg`-gated, not ported.** Tray, global-shortcut hotkey, autostart, and `notify-rust` get `#[cfg(desktop)]`/`#[cfg(windows)]`. **`winreg` is in the plain `[dependencies]` block today and will break the very first Android build** — it must move under `[target.'cfg(windows)'.dependencies]`. No autostart equivalent ships on Android v1. | §2.5; `ANDROID-PLAN.md` P1a |
| D24 | **Android secrets use the `keyring` crate's Android/Keystore backend**, not a hand-rolled store — its own Cargo feature **plus JNI `JavaVM`/`Context` init wiring** at startup. Its own spike inside Phase 7, not an assumed-trivial feature flip. **This is a silent-data-loss risk, not a build error:** with only `windows-native` enabled, keyring 3.x hits its catch-all `pub use mock as default` on Android and compiles fine — provider API keys appear to save, then vanish on relaunch. Nothing fails loudly. Phase 7 must not ship without it. | §10 P7 |
| D25 | **Android hardens the daemon's HTTP surface: `require_secret: true`, always.** Loopback is **not** process-private on Android — any app holding `INTERNET` can reach `127.0.0.1`. Also relax escalation-to-approval where the app sandbox *is* the security boundary. Both already flagged in code comments; neither had a decision until now. | §2.6, §10 P7 |

---

## 2. Architecture

### 2.1 Runtime split

| Platform | Chat | Summarizer | Embeddings | MCP tool servers | AP |
|----------|------|-----------|------------|------------------|----|
| Windows | Local (llama) **and** cloud providers | Local (llama) → session-model fallback | Local (via daemon endpoint) | stdio sidecars (bundled `.exe`s) | ✓ (in-process in daemon, semantic embeddings) |
| Android | **Cloud providers only** | Local (llama) → session-model fallback | **Local — same model, same endpoint** (D4 revised) | **the same crates, `in_process`** (§2.3) | ✓ (in-process in daemon, **semantic** embeddings) |

Since D4's revision the two platforms differ in exactly one place — whether
*chat* can run locally. Summarization, embeddings, and AP are now identical
code on identical models.

The MCP-tool-server column is the only row entry that needs new *wiring* rather
than new *code* — see §2.3.

### 2.2 Local model policy per platform (D18, as amended by D4's revision)
- **Android**: **two** resident local models — the summarizer (`LFM2.5-1.2B`,
  730 MB) and the embedder (`Qwen3-Embedding-0.6B` q4_k_m, 376 MB). Chat never
  loads a local model, so there are still no per-chat swaps. Both GGUFs are
  **downloaded on first use** (not bundled in the APK). Until each exists, its
  consumer degrades rather than fails: the summarizer falls back to the session
  model (D12), and AP falls back to hash-space embeddings (D4).
  - **This raises the Android RAM floor** — ~1.1 GB of weights plus KV/scratch,
    where the original plan budgeted for one model. The embedder is small and
    short-context (512 is plenty for a belief), so it is the cheaper of the two
    to keep resident; if a device can only hold one, evict per §4.1.
- **Windows:** 2 resident slots — `chat` and `summarizer`/`embeddings`. Chat
  slot only loads when a chat pins a local model (D2); otherwise chat talks to
  cloud providers and the slots stay on the summarizer + embedder.

### 2.3 In-process MCP + Adaptive-Pathway process model (D8)

**This mechanism already exists and is already used by four servers.** It is not
work this plan has to invent — the plan only has to *cross-compile* and
*register* it differently. Concretely, in `plugins/bigtiny_rust/`:

| Piece | File | What it does |
|---|---|---|
| `TransportType::InProcess` | `src/models/mcp.rs` | A fourth transport beside `Stdio`/`Sse`/`StreamableHttp`. Its DB row's `command` holds a **logical name** (`"kitty-tools"`, `"pathway"`, …), not an executable path. |
| `connect_in_process` | `src/mcp/client.rs` | Opens a `tokio::io::duplex` pipe, spawns the server future on one end, hands the other to `rmcp`'s client. `rmcp` is generic over `AsyncRead + AsyncWrite`, so a duplex stream is indistinguishable from a child's stdio. |
| `mcp::builtin::connect` | `src/mcp/builtin.rs` | Maps logical name → `serve_in_process` on the linked crate. `BUILTIN_SERVERS` lists them; the test `every_advertised_builtin_actually_connects` asserts every advertised name has a live match arm. |
| `serve_in_process` | each crate's `src/lib.rs` | `kitty-tools`, `kitty-web`, `kitty-wasm`, `adaptive-pathway` each expose a `[lib]` target wrapping the same server their `main.rs` serves over stdio. |

All four are already `{ path = ... }` dependencies of `bigtiny_rust`. `docs/PLUGINS.md`
("A third shape: in-process MCP server") is the canonical write-up.

**So the Android delta is narrow:**

1. Cross-compile the linked crates for `aarch64-linux-android` (Phase 1).
2. In `src-tauri/src/bigtiny/mcp.rs::ensure_builtin_servers`, register
   `kitty-tools`/`kitty-web`/`kitty-wasm` with `transport: "in_process"` and the
   logical name as `command`, instead of `transport: "stdio"` + a bundled exe
   path — **exactly how `"pathway"` is already registered there today.** Gate on
   platform; the desktop path is unchanged.

There is no third option to weigh: Android 10+ blocks `exec()` of anything in an
app-writable directory, which is *why* `InProcess` was built in the first place
(see the rationale comments in `models/mcp.rs` and `mcp/client.rs`).

**AP specifically** is never a standalone process on either OS — it's the linked
`adaptive_pathway` crate, reached both through the `in_process` `"pathway"` MCP
row and through direct `PathwayEngine` calls.

- **Windows:** the daemon itself is a Kitty-managed sidecar
  (`lifecycle/bigtiny_proc.rs`); AP lives inside it. Semantic embeddings come
  from `POST /api/embeddings` (daemon engine).
- **Android:** the daemon is in-process, so AP is naturally in-process too —
  **no code change separates the Android build from Windows** on the AP side;
  the only functional difference is hash-space embeddings (D4) since no embed
  model is loaded. Feature parity: recall, surfaced-assumption ordering,
  consolidation, PIP (confirm/sub/uncertain), suppression — identical behavior,
  just the vector space backing them differs.

### 2.4 Android tool surface

`kitty-tools` ships **22 always-on tools on desktop, 21 on Android** (`lean_shell`
is compiled out there), plus 3 viz tools behind `KITTY_VIZ_ENABLED`. The pinned
list lives in `plugins/kitty-tools/tests/protocol.rs::ALWAYS_ON_TOOLS`. Most need
nothing; the notes that matter:

- **`lean_shell` is already excluded on Android** —
  `#[cfg(not(target_os = "android"))]` in `plugins/kitty-tools/src/server.rs`,
  with the rationale in-place ("an app-sandbox shell backed by toybox isn't a
  useful `lean_shell` for a model to drive"). Nothing to do; don't re-add it.
  **But `ALWAYS_ON_TOOLS` in the test is *not* gated**, so
  `tool_surface_matches_env_gating` asserts a 22-tool surface unconditionally and
  will fail the moment that suite is run for an Android target. Gate the
  constant's `lean_shell` entry to match the server — a one-line fix, but a
  confusing failure if it's hit cold.
- **`lean_analyze_workspace` needs scoping.** It walks an arbitrary path, which
  on Android's scoped storage only usefully covers the app's own sandbox or a
  SAF-granted tree. Scope it to the app data dir on Android rather than letting
  it silently return almost nothing for a path the model picked.
- The file/Word/Excel/PDF/cache/scratchpad tools take explicit caller-supplied
  paths and work as-is once pointed at app-private storage — the same way they
  take an arbitrary Windows path today.

This is graceful degradation, not a blocker, and it is **not** a reason to
reintroduce a Python runtime: `kitty-docs-web` is retired, and its PDF/Excel/web
tools were reimplemented natively in Rust (`lopdf`, `calamine`, `scraper`+`htmd`)
precisely so no interpreter is needed. See §10 Phase 1 on `kitty-wasm`/wasmtime.

### 2.5 Desktop-only subsystems to gate (D23) — **DONE**

**Landed 2026-08-08.** `cargo ndk -t arm64-v8a --platform 26 check` on
`src-tauri` is clean (zero errors, zero warnings), with desktop unregressed
(138 tests, clippy clean, full `cargo build`). Recorded here as the map of what
was gated and why, since Phase 2 and Phase 6b both touch this surface again.

`tauri-plugin-single-instance` was **already** correctly gated (`Cargo.toml`
target table + `#[cfg(desktop)]` in `lib.rs`) and was used as the pattern
throughout.

| Subsystem | Where | What was done |
|---|---|---|
| `winreg` (autostart + Ollama env helper) | `Cargo.toml`; `wizard.rs`; **`config/env_helper.rs`** | Moved to `[target.'cfg(windows)'.dependencies]`; `cfg(windows)` on the autostart fns/consts/imports, `commands/setup.rs`'s two commands, `config/mod.rs`'s `env_helper` decl, `commands/ollama.rs`'s two env commands, and all four handler entries. **Two consumers, not one** — gating `wizard.rs` alone leaves the build broken. |
| **`screenshot.rs`** | `lib.rs`, `commands/mod.rs`, `commands/screenshot.rs`, `windows.rs::create_screenshot_select_window` + `wait_for_window_gone` | `#[cfg(windows)]` throughout. An entire ungated Win32 GDI module importing `windows::Win32::*` — a `cfg(windows)`-only dep. **The single biggest breaker, and originally unlisted here.** |
| Tray | `tray.rs`; `lib.rs` setup; **`notifications.rs::set_tray_pending`** | `#[cfg(desktop)] mod tray` + gated `tray::create` call. `set_tray_pending` keeps its signature (5 callers in `bigtiny/stream.rs` and `commands/session/prompt.rs`) with only its body gated — `tray_by_id` is itself `cfg(all(desktop, feature = "tray-icon"))`. The `tray-icon` feature is left on: gating the *module* is what matters. |
| Global-shortcut hotkey | `hotkey.rs`; `lib.rs` setup **and `commands/config.rs`** | Dep moved to the `cfg(not(any(android, ios)))` table; `#[cfg(desktop)] mod hotkey`; the plugin registration lifted out of the builder method chain into a `#[cfg(desktop)]` block (an inline `#[cfg]` mid-chain isn't valid). **`set_config` is a second `hotkey::register` call site** and must stay available on Android, so only its re-registration block is gated. |
| `notify-rust` | `notifications.rs` | **Already** in `[target.'cfg(windows)'.dependencies]` — the earlier claim that it sat in plain `[dependencies]` was wrong; only the call site needed work. `notify_if_hidden` keeps its signature and shared preamble, then delegates to a `#[cfg(windows)]` `notify-rust` arm (click-to-focus, `ToastJob` worker) or a **new** `#[cfg(not(windows))]` arm over `tauri-plugin-notification` — which was registered but had never been called from Rust. |
| Overlay window | `windows.rs::create_overlay` / `show_overlay`; `lib.rs` setup | `#[cfg(desktop)]` on the functions themselves, **not just the call site**: `decorations`/`always_on_top`/`skip_taskbar` are absent from the mobile `WebviewWindowBuilder`, so the body genuinely does not compile. `show_overlay` gets a `#[cfg(not(desktop))]` no-op arm so `complete_setup` needn't branch. |
| `keyring` (`windows-native` only) | `config/providers/` | **Left alone — it compiles** (see D24's mock fallback). Phase 7. |
| `externalBin` sidecars | `tauri.conf.json` → new **`tauri.android.conf.json`** | Tauri resolves sidecars by target triple, so the build demanded `binaries/kitty-*-aarch64-linux-android` and failed *in the build script*, before any Rust. A platform config override clears `externalBin`/`resources` for Android. Expected to be revisited in Phase 8; this is the minimum that unblocks compiling. |

Also swept up: `#[cfg_attr(not(desktop), allow(dead_code))]` on the chat-window
routing helpers (`show_and_focus`, `focus_or_open_session`,
`focus_or_open_chat_window`, `toggle_or_focus_main`, `any_open_chat_window`) —
live code with no Android caller *yet*, marked per-function rather than
blanket-allowed so Phase 6b can find them.

### 2.6 Android security posture (D25)

Two requirements, both already flagged in code comments but previously absent
from this plan:

- **`RunOptions::require_secret = true` on Android, always, with a generated
  secret.** The flag already exists (`plugins/bigtiny_rust/src/lib.rs`, threaded
  into the auth middleware's `required`); `src/server/middleware.rs` already says
  an embedding host on such a platform "should set this `true`". Nothing to
  build — just set it, and make sure a secret is actually generated and passed.
  Loopback is not process-private on Android: *any* installed app holding
  `INTERNET` can reach `127.0.0.1:<daemon port>`. On desktop the open
  `/api/health` readiness probe is fine; on Android an unauthenticated daemon
  means every other app on the device can drive the agent, read sessions, and
  invoke tools. This fails **silently** (nothing errors, nothing logs) if
  missed — hence the explicit Phase 7 acceptance criterion.
- **Relax escalation-to-approval where the sandbox is the boundary.**
  `plugins/bigtiny_rust/src/config.rs` already carries the note that the
  always-escalate-unrecognized-call check should be relaxed when "the daemon's
  data root is itself the security boundary (an app sandbox, e.g. Android)".
  Apply it for the Android build.

---

## 3. The local engine (`bigtiny_rust/src/local/`)

Lives **inside the daemon** (both OS). Entry points from `bigtiny_rust` crate.

### 3.1 Files

| File | Responsibility |
|---|---|
| `engine.rs` | `LocalEngine` wrapper over the `llama_cpp` crate. Builds the model/context from `LocalEngineConfig`: `n_ctx`, `n_batch`, `n_threads`, `cache_type_k/v`, FlashAttention (derived, D19). |
| `manager.rs` | Resident slot manager (§4.1). `load/unload/status`, hot-swap-queuing. |
| `provider.rs` | `LocalProvider: Provider` (base chat trait) — streaming + reasoning, text **and** compaction. |
| `embeddings.rs` | `LocalEmbed` → **desktop-only** endpoint `POST /api/embeddings` (D4). Android's AP uses the crate's built-in hash-space embeddings instead — no endpoint, no model. |
| `summarizer.rs` | Summarizer chain (§4.3). Grammar-constrained JSON decode. |
| `health.rs` | `/api/health` fields (`local` state, `model_backend`, `reload_required`, `restart_pending`) + `/api/local/models/status`. |

### 3.2 Config (in the daemon, `config.rs` + env-preserving)

```
[local]
enabled = true
default_model = "LFM2.5-1.2B q4_K_M"      # GGUF id, §9
n_ctx = 4096                                # desktop; Android default 2048
n_gpu_layers = "auto"                       # auto | -1 | 0 | N  (D20)
backend = "auto"                            # auto | cuda | vulkan | cpu
n_batch = 512
cache_type_k = "f16"                        # f16 | q8_0 | q4_0
cache_type_v = "f16"
```
`SummarizerConfig`:
```
[summarizer]
  fallback = "session_model"   # session_model | off  (D12, D13)
  temperature = 0.1
  reserve_exchanges = 3
  max_slot_items = 20
  timeout_s = 30
```
`flash_attn` is **not** a config key (D19).

### 3.3 Backend selection (D20)

`select_backend()` runs once at first engine use:

1. Query llama.cpp's ggml backend registry (no `nvidia-smi`, no subprocesses).
2. Return `Cuda` if a CUDA device is enumerated and compiled-in; else `Vulkan` if
   a Vulkan device exists; else `Cpu`.
3. `n_gpu_layers=-1`/`auto` → **all layers to the selected GPU backend**.
4. VRAM read for the fit/badge math uses the **selected backend's** memory bank:
   - `Cuda` → DXGI `IDXGIAdapter3::QueryVideoMemoryInfo(SEGMENT_LOCAL)`.
   - `Vulkan` → `vkGetPhysicalDeviceMemoryProperties` device-local heap.
   - `Cpu` → `0` (no GPU budget).
5. Fail-safe: on backend OOM at load, retry with `ngl` halved; at `0` → CPU.

#### Fit formula (autodetect/badge/card/RAM-warning — one shared function)

`required ≈ (file_size × resident_layer_fraction) + KV(n_ctx, cache_type) + scratch(n_batch, n_ctx)`

- `file_size` = on-disk GGUF bytes; `resident_layer_fraction = n_gpu_layers / n_layers`.
- `KV` is the KV-cache budget for the chosen `n_ctx` + `cache_type_k/v`.
- `scratch` = llama.cpp compute buffers for the given `n_batch` + `n_ctx` (CUDA
  scratch grows with both — a `file_size + KV` estimate alone **undercounts** on
  GPU offload).
- Multiply the total by **×1.18 (≈ +15–20% safety margin)** before comparing
  against free VRAM/RAM. This budget, not the raw file size, drives the
  "Recommended for this device" badge (§6.3), VRAM autodetect, and the
  low-RAM warning — preventing CUDA OOM on edge GPUs with high-context models.

Windows builds `llama_cpp` with `cuda`+`vulkan` features; Android builds CPU-only
(no Vulkan in v1; the same hook re-enables later).

---

## 4. Engine, slots, summarizer

### 4.1 Slots (from D2/D18/D21)

| OS | Slots | Content | Eviction |
|----|-------|---------|----------|
| Windows | chat pool + summarizer + embedder | chat pool: **one slot per active local-chat window** (D21) | idle slot only |
| Android | 2 | summarizer + embedder | embedder first under pressure (see below) |

**Embedder slot (D4 revised).** It is a separate slot from the summarizer, not
a share of it — different model, different `n_ctx` (512 is ample for a belief),
and `LlamaContextParams::with_embeddings(true)` is a context-construction flag,
so one context cannot serve both roles. It is also the cheapest thing to evict:
376 MB, short context, and AP already degrades gracefully to hash-space when it
is absent (D4). Under memory pressure on Android, **evict the embedder before
the summarizer** — a missed summarization blocks compaction, a missed embedding
just lowers recall quality for those beliefs until they are re-embedded by the
existing `reembed_stale_beliefs` pass.

- A session pins its model at creation (D2). A new session requesting a missing
  model **adds a chat-slot on demand** (Windows) or **queues behind any busy slot**
  (Android, where only the summarizer and embedder compete); frontend shows
  **"Model loading…"** in the new-chat composer until ready. In-flight streams
  are **never aborted**.
- **Windows multi-window (D21):** two hub windows may run **concurrently**, each
  with its own pinned (possibly different) model. Each gets its own chat slot from
  the pool; slots with identical model weights share the loaded tensors (only one
  copy of weights in memory, two contexts) when RAM allows.
- The summarizer slot reloads cheaply when the pinned chat model is also the
  summarizer default (shared weights).

### 4.2 Chat flows

- **Windows local chat:** pinned model → `LocalProvider` (chat slot).
- **Cloud chat (Windows + Android):** unchanged daemon provider registry —
  BigTiny talks to cloud providers directly; **no slot**, no local involvement.
- **Android chat: always cloud** (D18). The `hub` composer offers cloud providers
  only; there is no local-model picker on Android.

### 4.3 Compaction / summarizer chain (D12)

Order of attempts, first success wins; anything failure → next:

1. `Local summarizer engine` — load default model; run grammar-constrained JSON
   decode. On schema validation failure: one temperature-burn retry → non-grammar
   decode with refill prompt → fail.
2. **`summarizer.fallback`**:
   - `"session_model"` (default): call the chat's **pinned model** — may be cloud.
   - `"off"`: skip straight to 3.
3. **Explicit error** — surfaced in the UI; chat continues (compaction skipped this
   round, retried later).

Adjust: failure must **never block the chat session itself**.

---

## 5. Model downloader (Kitty core, shared Windows+Android)

Lives in Rust core (`commands/models.rs`), not the daemon. Uses `models://*`
events for progress.

### 5.1 Commands
- `list_local_models()` → installed GGUF list (+ size from GGUF header).
- `delete_model(id)` — manual only (D7).
- `get_disk_free()` — informational.
- `download_model(params)` — `{ id, source: "huggingface"|"ollama", url|repo, file|tag, rev? }`.

### 5.2 Download flow (both sources)

| Step | Behavior |
|---|---|
| **Gate** | Refuse if `free_space < expected_size × 1.5` (D7). |
| **Write** | `<model>.part` + `<model>.part.meta` (expected total, sha256). |
| **Resume** | `Range: bytes=<len>-`, seek to existing `.part` length. |
| **Verify** | sha256 on completion; mismatch → delete `.part`, auto-retry once — then explicit error. |
| **Finalize** | atomic rename `.part` → `.gguf`. |
| **Events** | progress via `models://*`; resumed offset re-emitted. |

- **HuggingFace**: `GET https://huggingface.co/{repo}/resolve/{rev}/{file}`; sha256
  from HF API.
- **Ollama registry**: `GET https://registry.ollama.ai/v2/{ns}/{name}/manifests/{tag}`;
  fetch blobs in order, concatenate → `.gguf`.
  - **Per-layer validation:** before streaming each blob, check `Content-Type`
    **and** gzip magic bytes (`0x1f 0x8b`). Some manifests / registry versions
    serve **gzip-compressed layers**
    (`application/vnd.ollama.image.layer.v1.tar+gzip` or plain gzip) — when
    detected, decode through a **streaming `flate2` GzDecoder** into `.part`
    instead of copying raw bytes.
  - **Digest rule:** Ollama layer digests may be computed over the compressed
    stream. Verify the digest of each blob as actually stored (compressed); the
    final sha256 gate runs over the **decoded, concatenated `.gguf`** at
    finalize. Mismatch anywhere → treat as a download error (delete → retry once).
- Low-free-space **warning** (~<2 GB free) in the Settings UI.
- Queue + cancellation; on Android, downloads run under the **foreground service**.
- **Android GC, doze & network handoff:** the FGS runs with `dataSync` type and a
  visible notification; it holds a `ConnectivityManager` `NetworkCallback` to
  survive Wi-Fi↔Cellular switches — on a switch, **re-verify the byte offset**
  (`Range`/`Accept-Ranges`/ETag) before resuming, and re-queue with
  `setRequiredNetworkType(NetworkType.UNMETERED)` if the download policy requires
  metered-safe behavior (WorkManager option only if we adopt it). Deep Doze must
  never abort a `.part`; offset resume in §5.2 makes interrupted downloads
  continue from the last verified byte.

---

## 6. Settings surface (`Phase 4`)

All under `Settings → Local models`, shared component on both OS.

### 6.1 Engine knobs (Windows shows GPU/CPU; Android hides GPU knobs per D19/D20)

| Group | Knobs |
|---|---|
| Context | `n_ctx` |
| Performance | `n_gpu_layers` (auto/-1/0/N), `n_batch`, `n_threads` (CPU) |
| KV cache | `cache_type_k`, `cache_type_v` (f16/q8_0/q4_0) |
| Sampling | `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty` |
| More | `timeout_s` |

- Android: only `n_ctx` + sampling + presets shown; GPU/`n_gpu_layers`/`n_batch`
  hidden (CPU-only). UI is backend-aware.

### 6.2 Presets (D6)

| Preset | temp | top_k | top_p | repeat_penalty |
|---|---|---|---|---|
| Precise | 0.1 | 50 | 0.95 | 1.05 |
| Balanced | 0.6 | 40 | 0.9 | 1.05 |
| Creative | 1.0 | 0 (off) | 1.0 | 1.1 |

### 6.3 Model card + health + badge (D10/D13)

| Field | Source |
|---|---|
| File size | GGUF header |
| RAM | §3.3 fit formula (file + KV + scratch, ×1.18) for `n_ctx` |
| VRAM | §3.3 fit formula against the active backend's memory bank |
| Backend now | `/api/health.model_backend` |
| FlashAttention | derived diag (`on (<backend>)` / `off`) |
| Health dot | `/api/local/models/status`: `ready`=green, `download_failed`/`load_failed`=red, `slot_busy` |
| Badge | `Recommended for this device` — computed by the **same** §3.3 fit fn (never disagrees) |

- The badge appears on Windows for every listed GGUF; on Android it applies to
  the single summarizer.

### 6.4 Automatic engine restart on config change (D11)

- Changing `n_ctx`, `n_gpu_layers`, KV cache, `n_batch` (load-time params) marks
  the model `reload_required` **and** schedules a restart.
- **Restart timing:**
  - If **no** LLM generation is in flight → **restart immediately** on change.
  - If a generation is in flight → the restart is **queued until the current
    output finishes**, then applied. New generations started meanwhile are
    admitted (they'd run on the old engine; a queued restart picks them up on
    the reload).
- Sessions bound to the affected model show a non-blocking **"restarting…" chip**
  (never a blocking modal); the daemon exposes the pending restart in
  `/api/health` (`reload_required` + `restart_pending`). Nothing fails silently.

---

## 7. Scheduled tasks (D3)

- `ScheduledTask` (daemon `config.rs`) gains `model_id: Option<String>` (empty =
  summarizer default).
- Fire: pass `model_overrides` on `send` (`PATCH /api/chat/{id}/model`); else the
  default local model.
- `commands/scheduled_tasks.rs` + `ScheduledTasks.tsx` picker — **Windows only**
  shows local models; Android picker lists cloud providers + the summarizer.

---

## 8. Frontend

### 8.1 Windows build (Phase 6a)
- Vite multi-page: `['overlay','hub']` (remove windows `settings`,`wizard`,
  `screenshot-select` for new desktop; `screenshot-select` folded into hub as an
  in-window route).
- New `src/windows/hub/` → `<HubApp>` routing `chat | sessions | settings | wizard`
  via a tiny zustand `routeStore`.
- Model choice lives **only** in the new-chat composer (D2).
- **Multi-window (D21):** the hub window may be instantiated **more than once** —
  each instance is an independent viewer with its **own session and its own pinned
  model**. `open_new_chat_window`/`open_main` create a new hub instance instead of
  reusing an existing one when a distinct session is requested. `route://goto`
  emits + `focus_or_open` target a specific instance; `get_settings_target`
  returns a route payload.

### 8.2 Android shell (Phase 6b)
- Same `<HubApp>`; bottom tabs (**Chat / Models / Settings**), share intent,
  embed/back; CSS `<480px` + safe-area. `ipc.ts` add `navTo(view)`, `shareText`.
- No local-model composer on Android (D18): the tab shows the single summarizer
  card (read-only) + cloud providers for chat.

### 8.3 Wizard (D14)
- One shared, in-page step flow. Adapter:
  - Windows: daemon present? → autostart → done.
  - Android: **permissions step** (POST_NOTIFICATIONS, foreground service) — only
    platform divergence.

### 8.4 Tokens / theme / safe-area / fonts (D15–D17) — Phase 6b
- Extend `src/themes/` contract with a **semantic token ramp** (spacing/type/status)
  + `--safe-top/right/bottom/left`.
- `system` theme mode = `default` under `prefers-color-scheme: light`, `dark`
  under `dark`. Manual themes unchanged.
- Safe-area: `viewport-fit=cover` + `env(safe-area-inset-*)` **only** on Android;
  desktop reads the same tokens at `0`.
- **`--font-scale` at `:root`**, consumed via `rem` — Android font-scale and
  Windows zoom both write into it → shared breakpoints.
- **Full px→rem sweep of `base.css`** (~110 hardcoded font sizes) in one pass.

---

## 9. Default models

### 9.1 Summarizer

- `LiquidAI/LFM2.5-1.2B-Instruct-GGUF` → file
  **`LFM2.5-1.2B-Instruct-Q4_K_M.gguf`** — note the case; HuggingFace resolve
  URLs are case-sensitive and the all-lowercase name this doc used to give
  404s.
- **Verified in Phase 1** against `llama-cpp-2` 0.1.154: arch `lfm2` loads and
  generates. Measured from the GGUF, superseding this section's earlier
  estimates: **730,895,168 bytes** on disk, 16 layers, `n_embd` 2048, vocab
  65,536, and **`n_ctx_train` = 128,000** (not 32k). CPU compute buffer at
  `n_ctx` 2048 was ~142 MiB.
  - Baseline sampling = the Precise preset.
  - **It is instruct-tuned and needs its chat template.** A bare prompt makes
    it emit EOS immediately — zero tokens, which looks exactly like a broken
    build. `LocalProvider` must go through `LlamaModel::chat_template` +
    `apply_chat_template` and tokenize with `AddBos::Never` (the template
    already carries the BOS/turn markers).
- **Fallback model = Qwen3-1.2B q4_K_M** — not needed; `lfm2` works. Kept as
  the documented escape hatch only.

### 9.2 Embedder (D4 revised — both platforms)

- **`Qwen3-Embedding-0.6B`, q4_k_m GGUF — validated.** 394,704,832 bytes
  (376 MiB), **1024-dim**, `n_ctx_train` 32,768, 28 layers. One model on
  Windows and Android, so beliefs embedded on one platform stay comparable on
  the other.
  - `sha256 = c608745221a03d45ee7328aab5ae180ef5db54c9a47eda43ef05f73156ba824b`
    — pin this in the Phase 3 downloader.
  - Measured separation on a related/unrelated pair: **cos 0.73 vs 0.29**. The
    probe (`examples/local_embed_spike.rs`) asserts that ordering, not just the
    vector shape — a backend returning well-formed constant vectors would pass
    a dim check while silently destroying AP recall.
- **Pooling must be set explicitly, per model.** llama.cpp defaults to
  `LlamaPoolingType::None`, which yields *no* sequence embedding —
  `embeddings_seq_ith` then fails with `NonePoolType`. Qwen3-Embedding is a
  causal-LM-derived embedder and needs **`Last`**. A BERT-style embedder (bge,
  gte, nomic) would need `Mean`/`Cls` instead, so this is a property of the
  model pin, not a constant in the engine — `embeddings.rs` must carry it
  alongside the model id. (It fails loudly, which is the good case.)
- **Source caveat, worth knowing:** Qwen's *official* GGUF repo
  (`Qwen/Qwen3-Embedding-0.6B-GGUF`) publishes **only `Q8_0` (609 MB)** — no
  smaller quant. The 376 MB q4_k_m comes from the community repo
  `Mungert/Qwen3-Embedding-0.6B-GGUF`. Since this is a bundled default rather
  than a user-chosen model, **pin its sha256** in the Phase 3 downloader (D5
  already verifies sha256) so provenance is a fixed artifact rather than
  standing trust in an uploader. If that repo ever disappears, official `Q8_0`
  is the drop-in fallback at +233 MB.
- The base `Qwen/Qwen3-Embedding-0.6B` repo is safetensors only, and
  `mlx-community/...-4bit-DWQ` is **MLX format — Apple-Silicon only, not
  loadable by llama.cpp**. Neither is usable here.
- Dimension mismatch is a non-issue: adaptive-pathway already projects to its
  configured `embedding_dim` (`embed/project.rs`), and changing embedder
  triggers the existing `sync_embedding_model_fingerprint` → re-embed
  migration rather than silently mixing spaces.
- GGUF cache lives in app-local dir (Android backup-excluded).

---

## 10. Phases & acceptance criteria (Do this order)

### Phase 1 — Cross-compile spike + Android toolchain bounds
- **Goal:** prove `llama_cpp` + `wasmtime` + `sqlx` cross-compile for
  `aarch64-linux-android`; pin the llama.cpp binding crate (D1); confirm arch
  `lfm2` loads; verify `-1` behaves as "all layers" across packaged backends.
- **Prerequisite: DONE.** The D23 gating (§2.5) landed 2026-08-08 and
  `src-tauri` now cross-compiles clean for `aarch64-linux-android`. The
  toolchain (D22) is installed. Phase 1 starts from a working runway.
- **Why `wasmtime` is on this list** (previously unstated): it backs
  `kitty-wasm`, the fourth in-process MCP builtin (§2.3), which hosts the
  sandboxed code-execution tools (`wasm_python_run` et al.) via a pinned CPython
  3.12 WASI guest. It is **not** a Python-hosting layer for the retired
  `kitty-docs-web` — those tools are native Rust now (§2.4).
- **Also cross-compile `kitty-tools` and `kitty-web`** — they are `bigtiny_rust`
  path-deps too (§2.3). Both are pure Rust with no native-toolchain risk, so
  they should be uneventful; they're named here so "the daemon builds" actually
  means all four linked crates build.
- **Toolchain/linkage constraints (Android):** the NDK+CMake result must be
  **statically linked** into the single Rust cdylib shipped in the APK —
  `ANDROID_STL=c++_static`, `libgcc`/`libunwind`/`libatomic` resolved statically,
  and **`LLAMA_OPENMP=OFF`** for the Android target (a libgomp runtime dependency
  is a missing-symbol hazard on varied OEM devices). No helper `.so` may be
  `dlopen`'d at runtime.
  - If the build nonetheless leaks an extra JNI `.so`, it must be loaded with an
    explicit `System.loadLibrary` in `gen/android` Kotlin **before** the Rust
    runtime initializes.
- **Acceptance:** `cargo build --target aarch64-linux-android` for `bigtiny_rust`
  with llama + wasmtime + sqlx **+ all four linked path-dep crates
  (`adaptive-pathway`, `kitty-tools`, `kitty-web`, `kitty-wasm`) — no separate
  artifact for any of them**; a `LFM2.5-1.2B-Instruct-Q4_K_M.gguf` load via the pinned binding
  returns tokens; **app boots on an API 26–34 emulator AND one physical arm64
  device with zero `UnsatisfiedLinkError`/`dlopen` failures**; `readelf -d`/`nm -u`
  on the produced `.so` confirms no unresolved external symbols. Record the pinned
  NDK version back into D22. Fallback decision tombstones in `docs/ANDROID.md` if
  `lfm2` fails.

### Phase 2 — Daemon engine (Windows first) — **SPLIT into 2a / 2b**

Phase 2 as originally written bundled "add the local engine" with "delete
Ollama". Those are separable and are now sequenced, so there is always a
working state to fall back to:

**Phase 2a — add the engine alongside Ollama (nothing breaks).** Build §3's
`bigtiny_rust/src/local/` (engine/manager/provider/embeddings/summarizer/
health), the `[local]`/`[summarizer]` config, `select_backend()` (D20), the
§3.3 fit formula, and the **new `POST /api/embeddings` route** — deliberately
response-compatible with Ollama's (`{model,prompt}` → `{embedding:[…]}`) so
2b's re-point stays a one-line change. Ollama is untouched; `StackStatus`
*gains* a variant rather than losing one. Independently shippable, and the
point at which the local engine can be compared against Ollama on real
hardware before anything is deleted.

**Phase 2b — retire *managed* Ollama.** Only after 2a is green, and in this
order: flip adaptive-pathway's `AP_EMBED_OLLAMA_URL` to the daemon
(`lifecycle/bigtiny_proc.rs`), confirm recall still works, *then* delete.

> **Scope of "no Ollama" (decided).** It means **Kitty manages no inference
> process** — no spawned `ollama serve`, no pull API, no `ollama://pull-progress`,
> no HKCU env editor, no keep-alive warm/evict, no wizard install steps, no
> Ollama settings tab. It does **not** mean forbidding Ollama as an endpoint:
> `provider_type: 'ollama'` stays selectable in `ProviderForm`, along with its
> `provider/sampling.rs` defaults arm and the `top_k`/`min_p` wire gate, so
> pointing at an Ollama server you run yourself keeps working. The daemon
> already collapses every OpenAI-compatible dialect into `openai_compat`, so
> this costs essentially nothing.

- **2a acceptance:** `cargo test` + `cargo clippy` clean; a chat turn served by
  `LocalProvider`; compaction served by the local summarizer returning
  schema-valid JSON; `/api/embeddings` round-trips at the configured dim; AP
  (recall/consolidate/surface) still testable in-process **without**
  `/api/embeddings` (the hash-space path Android uses); **Ollama paths still
  work unchanged**.
- **2b acceptance:** AP verified against the daemon endpoint *before* deletion;
  `grep -ri ollama` shows only the intentionally-retained dialect surface; the
  rewritten `stack_needs_ollama` tests pass.
- Files: §3 + deletion of `src-tauri/ollama/`, `commands/ollama.rs`,
  `lifecycle/ollama_proc.rs`, `lifecycle/summarizer_model.rs`,
  `config/env_helper.rs`, `state.ollama`, `providers::active_ollama_target`.
  **Rewrite rather than edit:** `lifecycle/mod.rs` (`start_stack` step 1 + the
  three-way `stack_needs_ollama` rule — real product logic, and its 4 tests get
  rewritten to assert the new rule, not deleted) and `lifecycle/embedding.rs`
  (~70 of 104 lines are Ollama-pull-shaped).
- **Existing-install migration.** `ProviderProfile.provider_type` is an untyped
  `String` (confirmed by the existing `old_shape_provider_migrates_with_defaults`
  test), so a saved `"ollama"` profile keeps deserializing fine after removal —
  **no crash, but no function either**: it silently becomes an unreachable
  provider once `active_ollama_target` and the sidecar are gone. Per CLAUDE.md
  rule 6 ("errors are states, not toasts"), surface it as an explicit *removed
  provider* state in Settings → Providers with a delete/replace action, rather
  than leaving a dead row the user has to figure out. **Acceptance:** loading a
  pre-migration `config.json` containing a `provider_type: "ollama"` profile
  produces that visible state and never a hard error.

### Phase 3 — Downloader
- **Acceptance:** `download_model` (HF + Ollama registry) tests: resume-after-
  kill, sha256-mismatch retry-then-fail, atomic rename, 1.5× refuse gate, a
  **gzip-compressed Ollama layer** fixture (decodes via flate2; content-digest
  vs decoded-GGUF verification), and an Android **byte-offset resume on
  connectivity drop** test.

### Phase 4 — Settings
- Knobs/presets/model card/badge/health; **auto-restart scheduling** (§6.4);
  backend-aware hiding. Acceptance: settings round-trip via `commands/` + UI;
  **restart applies immediately when idle, and only after the in-flight
  generation completes when busy** (verified with a long-running stream); no
  silent load-param failures.

### Phase 5 — Scheduled tasks overrides

### Phase 6a — Desktop hub (land green)
- Wrap existing window components into `hub`; Vite intake change; routes;
  **multi-instance hub (D21)**. Acceptance: overlay + hub behave pre-restructure;
  regression via `pnpm test`; **two hub windows open with two different sessions
  and two different pinned models generate concurrently** (each with its own chat
  slot).

### Phase 6b — Android shell + tokens + scale
- Mobile shell, tokens/theme `system`, safe-area, full px→rem sweep.
- **Appearance-parity** screenshot check: overlay + hub on both OS.

### Phase 7 — Android native
- `KittyForegroundService` (`dataSync`, `START_STICKY`, POST_NOTIFICATIONS grant
  [wizard §8.3]), `SecretStore` (Keystore backend for provider keys/HF token),
  GGUF first-use path, no local chat picker, backward-compat.
- **`SecretStore` = `keyring`'s Android backend (D24), not a bespoke store** —
  its own Cargo feature **plus** JNI `JavaVM`/`Context` init wiring from Tauri's
  Android runtime before any `keyring::Entry` call. Budget this as a spike: the
  existing `migrate_secrets`/`get_or_create_bigtiny_encryption_key` helpers
  assume a single backend shape and will need the init to have already run.
- **Daemon hardening (D25, §2.6):** `require_secret: true` with a generated
  secret; relaxed escalation-to-approval for the sandboxed data root.
- **Tool surface:** apply §2.4's `lean_analyze_workspace` scoping;
  `lean_shell` is already excluded, leave it that way.
- **Download-while-backgrounded (doze + network):** service exposes a dataSync
  FGS + visible notification, a `ConnectivityManager.NetworkCallback` for
  Wi-Fi↔Cellular handoff, and byte-offset resume (§5.2) — a multi-GB GGUF pull
  survives Deep Doze and network changes without corruption.
- **In-process AP (D8):** the daemon's `PathwayEngine` runs end-to-end on Android
  — recall woven into turn processing, assumption surfacing, consolidation — all
  with hash-space embeddings; no `externalBin`, no separate process, no
  Ollama-URL config to resolve.
- Accept: build+install on device; summarizer cloud fallback fires; wizard grants
  perms; OEM battery restart resumes; **a model download interrupted by airplane
  mode resumes from the same byte after reconnect**; **AP recall/surface/consolidate
  verified on-device (hash-space) and on desktop**; **a second app on the device
  cannot reach the daemon's `/api/*` without the secret** (D25 — verify with a
  plain `curl` from an adb shell, which is the same unprivileged position any
  other installed app is in).

### Phase 8 — Packaging
- `plugins/build.py` must NOT freeze Rust sidecars for Android. `externalBin`
  removed for Android; daemon + kitty-* linked in (AP comes along inside the
  daemon crate — no separate artifact). Signed AAB + Windows installer.
- `AGENTS.md` commands gain an `android` lane; scrub OLLAMA references in `docs/`.
- **Update `CLAUDE.md`'s framing.** Its opening line and tech-stack section
  currently say "Windows-only Tauri v2 desktop app" / "Windows-only target",
  which stays *accurate* right up until this phase — so it is deliberately
  **not** touched earlier. Once Android ships, rewrite those to the dual-target
  reality (and the Ollama references, already stale after Phase 2). Same
  treatment `goose-overlay-project-description.md` got when the backend swapped:
  supersede it, don't quietly leave it wrong.

---

## 11. Risks / notes

- ~~Which binding crate~~ — **settled in Phase 1: `llama-cpp-2` 0.1.154.** It
  builds, links, loads `lfm2`, and generates on Windows. The `LocalEngine`
  boundary still isolates a future swap.
- **Build prerequisites this doc never mentioned**, all discovered the hard way
  in Phase 1, all required by `llama-cpp-sys-2`, and every one of them fails
  in a way that reads like a broken crate rather than a missing tool:
  - **CMake** — configures and builds llama.cpp from source. First Windows
    build ~4.5 min; incremental is cheap. Budget for it in CI.
  - **libclang**, for `bindgen`. **Nothing on a stock Windows dev box has
    one** — not the NDK, not Visual Studio. `winget install LLVM.LLVM` is the
    clean fix but needs elevation; set `LIBCLANG_PATH` if you source it
    another way. Failure mode: a bindgen panic.
  - **Ninja**, for Android only — and this one is a real trap.
    `llama-cpp-sys-2`'s `build.rs` sets `CMAKE_TOOLCHAIN_FILE`, `ANDROID_ABI`,
    `ANDROID_PLATFORM` and `ANDROID_STL` correctly, but **never sets a cmake
    generator**. On Windows the `cmake` crate then defaults to Visual
    Studio/MSBuild, which tries to build llama.cpp for **x64 MSVC** and dies
    on `VCTargetsPath`/`MSB1009`. Set **`CMAKE_GENERATOR=Ninja`**.
  - **NDK env var names.** `build.rs` reads `ANDROID_NDK`, `ANDROID_NDK_ROOT`,
    `NDK_ROOT`, or `CARGO_NDK_ANDROID_NDK` — **not `ANDROID_NDK_HOME`**, which
    is what cargo-ndk and most guides tell you to set. There's an
    `ANDROID_HOME/ndk/*` fallback, so it often works by luck; set the explicit
    names rather than relying on it.
  - **Stale cmake caches are sticky.** After a failed configure, the crate
    logs `CMake project was already configured. Skipping configuration step.`
    and reuses the *wrong* generator forever. Fixing the env is not enough —
    delete `target/<triple>/*/build/llama-cpp-sys-2-*/` before retrying.
- **§11's cmake-variable framing is superseded by cargo features.** With
  `llama-cpp-2` you do *not* set `ANDROID_STL`/`LLAMA_OPENMP` by hand:
  - `ANDROID_STL=c++_static` → the **`android-static-stdcxx`** +
    **`static-stdcxx`** features.
  - `LLAMA_OPENMP=OFF` → simply **do not enable the `openmp` feature**.
  - **`default-features = false` is mandatory**, and is the whole reason: the
    crate's default set is `["openmp", "android-shared-stdcxx", "common"]`,
    which is wrong for us on both counts (OpenMP on, and *shared* libc++ —
    the opposite of the single-self-contained-cdylib rule below).
- ~~`winreg` breaks the first Android build~~ — **resolved 2026-08-08** along
  with the rest of D23/§2.5. Worth remembering the shape of it, though: the
  failure presented as a toolchain problem and was a one-line manifest fix, and
  the two biggest offenders (`screenshot.rs`, `config/env_helper.rs`) weren't in
  the original list at all. Expect the same when Phase 2 and Phase 6b touch this
  surface: grep for the *second* consumer.
- **The Android loopback exposure fails silently** (D25/§2.6). If
  `require_secret` isn't set, nothing errors, nothing logs, and every test still
  passes — the daemon is simply reachable by every other app on the device.
  There's no natural moment it would be noticed, which is why it's an explicit
  Phase 7 acceptance check rather than a note.
- **Already de-risked — don't re-litigate:** `reqwest` is configured
  `default-features = false, features = ["rustls-tls", …]` across the workspace,
  with no `native-tls`/`openssl`/`openssl-sys` anywhere in `bigtiny_rust`'s
  lockfile. That sidesteps cross-compiling OpenSSL for `aarch64-linux-android`
  entirely. Keep it that way; a well-meaning switch to `native-tls` would
  reintroduce a real Phase 1 blocker.
- **NDK C++/libatomic linkage is a Phase 1 hard gate:** static `c++_static` +
  statically linked `libgcc`/`libunwind`/`libatomic`, `LLAMA_OPENMP=OFF` on
  Android, single cdylib, no leaked JNI `.so` — otherwise `UnsatisfiedLinkError`
  / `dlopen` failures on varied OEM devices. Verified on emulator + physical
  arm64 device.
- Summarizer JSON grammar decode is the touchiest edge — burn-retry + refill +
  fallback cover it.
- **Ollama registry layers can be gzip-compressed** on some manifests/versions —
  handled by streaming `flate2` per-layer (§5.2); keep the digest-vs-decoded-GGUF
  split in mind when extending.
- Android summarizer downloads run under the foreground service but `AICore`-type
  system veto rarely applies (no Gemini). OEM battery managers plus **Deep Doze
  and Wi-Fi↔Cellular transitions** may stall long GGUF pulls → `START_STICKY` +
  persisted queue + byte-offset resume on reconnect (§5.2/§7).
- The narrow cloud exception (D12) is deliberate; keep it scoped to compaction.
- ~~AP on Android uses hash-space embeddings, so recall quality lags Windows~~
  — **retired by D4's revision.** Both platforms now run the same
  `Qwen3-Embedding-0.6B`, so there is no quality gap and no cross-platform
  vector-space split. The trade moved rather than vanished: Android now carries
  **~1.1 GB of resident model weights** instead of one model, which is the new
  thing to watch on low-RAM devices (§2.2, §4.1's eviction order).
- **Hash-space is still reachable and still matters** — it's the pre-download
  and load-failure fallback (D4). Beliefs written in that space are tagged
  `HASH_EMBED_MODEL` and picked up later by `reembed_stale_beliefs`, so a
  first-run device that embeds before the GGUF lands self-heals. That machinery
  already exists and was hardened in the 88bugs re-audit (#89/#90).
- **Windows multi-window RAM:** two concurrent chat windows pin two potentially
  different local models → two chat slots + summarizer resident. Weights are
  shared when models match; the chat-pool evicts the **idle** slot first if RAM is
  tight, and the fit/badge math (§3.3) underpins a low-RAM warning.