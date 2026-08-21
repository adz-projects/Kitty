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
- **Phase 1 (1-device) — PASSED 2026-08-09** on a **Pixel 10 Pro (arm64-v8a,
  Android 16 / API 36)**. Both spikes were cross-compiled as native ARM64
  binaries, pushed to `/data/local/tmp`, and run:
  - Summarizer produced **byte-identical output to Windows**, in **3.4 s wall**
    including the full 730 MB model load. On-device summarization is viable.
  - Embedder returned 1024-dim vectors, cos 0.7425 / 0.2922 (Windows: 0.7325 /
    0.2936 — SIMD-path variance, not a behavioural difference).
  - **`nm -u` criterion now genuinely met.** Building the examples *did* run
    the linker, so this is no longer deferred: the ELF's only `NEEDED` entries
    are `libdl.so`, `libm.so`, `libc.so`. **No `libc++_shared.so`** — the
    `static-stdcxx` feature works, confirming §11's single-self-contained-
    artifact rule. All 168 undefined symbols are bionic libc, resolved by the
    platform. Zero `UnsatisfiedLinkError`/`dlopen` failures.
- **Still open from Phase 1:**
  - ~~**D20 backend selection unvalidated**~~ — **closed.** Vulkan is enabled
    on Windows and validated end to end on a GTX 1650 Ti: 17/17 LFM2.5 layers
    offloaded, `fit_params` driving the choice. See §3.3.
  - The link evidence above is from an *executable*. The Phase 7 in-process
    **cdylib** is a different link, and should be re-checked the same way when
    it exists.

---

## 1. Decisions (definitive register)

| # | Decision | Where it lands |
|---|----------|----------------|
| D1 | **In-process llama.cpp, day 1, both OSes.** `bigtiny_rust` links **`llama-cpp-2` (`utilityai/llama-cpp-rs`), pinned at 0.1.154** — chosen in Phase 1 and validated end-to-end. The `llama_cpp` crate this doc used to name as a placeholder is **abandoned** (last release 2024-04-29). The `LocalEngine` boundary (§3.1) keeps the binding swappable. Chat, compaction, and desktop embeddings all go through the daemon; Kitty manages no inference process of its own (see §10 Phase 2 for what "no Ollama" does and doesn't mean). | §2, §4, §11 |
| D2 | **Model is pinned per chat.** Locked in at chat creation; changes only on **new chats**. No mid-stream swap, ever. | §4.1 |
| D3 | **Scheduled tasks default to the summarizer model**; per-task override in Settings. | §7 |
| D4 | **REVISED 2026-08-09 — semantic embeddings on BOTH platforms, one shared model.** `Qwen3-Embedding-0.6B` (official `Q8_0` GGUF, 609 MB, 1024-dim) runs through the same in-process engine on Windows *and* Android. AP calls the new `POST /api/embeddings`; `AP_EMBED_OLLAMA_URL` re-points to the daemon port. The hash-space vectorizer (`HASH_EMBED_MODEL`) is demoted to a **fallback** — used before the model is downloaded, or if it fails to load — not the Android norm. *Supersedes the original "desktop only / Android is hash-space" decision; §11's recall-quality-gap risk is retired with it.* | §3.1, §9 |
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
| D24 | ~~**Android secrets use the `keyring` crate's Android/Keystore backend**, not a hand-rolled store — its own Cargo feature plus JNI init wiring.~~ **Revised and closed 2026-08-11: there is no such backend.** keyring 3.6.3's feature list is `apple-native` / `linux-native` / `windows-native` and nothing else, so the premise of this decision was simply wrong — there was no feature to enable. The diagnosis was right, though, and worth keeping: with only `windows-native`, keyring hits its catch-all `pub use mock as default` on Android and compiles fine, so provider API keys appeared to save and vanished on relaunch with nothing failing loudly. **Actual fix:** `gen/android/.../SecretStore.kt` (AES-256-GCM under a non-exportable AndroidKeyStore key, sealed blobs in private SharedPreferences) behind `src/android/secrets.rs`, dispatched from `config::providers::keyring`. `keyring` is now confined to the non-Android target table so the mock cannot return. | §10 P7 |
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
  730 MB) and the embedder (`Qwen3-Embedding-0.6B` `Q8_0`, 609 MB). Chat never
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
| `KITTY_PLUGIN_HOME` | `src-tauri/src/lifecycle/bigtiny_env.rs` | **Android-only.** Where the three tool plugins put their caches, and what their path-containment checks treat as "inside home". Set to `config::config_dir()`. See §2.4a for why they cannot work it out themselves. |

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
  `ALWAYS_ON_TOOLS` in `tests/protocol.rs` carries the same `cfg`, so the
  suite asserts the surface the server actually advertises on each target.
- **`lean_analyze_workspace` is scoped — DONE.** It walks an arbitrary path,
  which under scoped storage is unreadable or nearly empty, and a near-empty
  listing reads to the model as "this directory is empty" rather than "you
  cannot see this". `workspace.rs::scoped_root` redirects a root outside the
  app's storage to `KITTY_PLUGIN_HOME` on Android; the response's `root` says
  which directory was actually walked.
- The file/Word/Excel/PDF/cache/scratchpad tools take explicit caller-supplied
  paths and work as-is once pointed at app-private storage — the same way they
  take an arbitrary Windows path today.

### 2.4a Where the tool plugins write (D-plugin-paths) — **DONE**

**Landed 2026-08-21.** The three plugins were written for the desktop hosting
model — a stdio child with a real `$HOME` and a working `/tmp`. Hosted
in-process on Android they have neither: the app process has no useful `$HOME`
(bionic's `getpwuid` reports `/data`), no `/tmp` and no `TMPDIR`, and a working
directory of `/`. Five directory helpers resolved somewhere unwritable, which
is what broke PDF scraping and reading, the search offload store, the
scratchpad, the whole cache tool family, and the `kitty-wasm` guest download
this platform's design depends on.

`KITTY_PLUGIN_HOME` (§2.3) is the answer: one variable, set once, from which
each crate derives its own directories.

```
paths::home_dir()  ->  KITTY_PLUGIN_HOME, else USERPROFILE, else HOME, else dirs::home_dir
cache_dir()        ->  <home>/.cache/lean-goose-mcp        (kitty-tools, kitty-web)
search_store_dir() ->  <home>/.cache/kitty-search-offload
guest::data_dir()  ->  KITTY_WASM_DATA_DIR, else <home>/.kitty-wasm
guest::run_dir()   ->  <data dir>/run                      (replaces std::env::temp_dir)
```

Three things about it are load-bearing:

- **It is set in `daemon_env`, not `bigtiny::mcp::server_env`.** The latter
  calls `set_var` from `sync_mcp_once_healthy`, i.e. after the daemon's tasks
  are already running, which is not sound against a concurrent reader.
  `bigtiny_embedded::start` applies `daemon_env` before the daemon exists.
  Don't add new variables to `server_env`.
- **It is unset on desktop.** `~/.cache/lean-goose-mcp` is shared with users'
  existing cached data and with the retired Python tools; overriding it there
  would orphan it.
- **There is no working-directory fallback any more, on any platform.** There
  used to be, and it silently inverted the containment boundary: with home
  resolved as `/`, `path_within_home` admitted every path on the device — so
  `kitty-wasm`'s `workspace` could have mounted anything read-write into the
  guest, the hole audit #111 closed. A boundary that cannot be located now
  rejects, and `home_dir()` returns `Option` so that is unmissable at each
  call site.

`kitty-wasm` needed three more things to be viable here, recorded because they
are not obvious from the code: the compile cache loads via `Module::deserialize`
from bytes rather than `deserialize_file`, because the latter mmaps the
`.cwasm` and `mprotect`s it `PROT_EXEC` and Android's SELinux policy denies
`file { execute }` on `app_data_file` (anonymous mappings only need
`process { execmem }`, which app processes have); the guest linear-memory
ceiling drops from 512 MB to 128 MB, because a guest allowed to grow past what
the device will give one app gets *Kitty* OOM-killed rather than trapping
cleanly; and the guest download is `Content-Length`-checked and capped rather
than buffered whole.

**Still unverified on hardware:** whether wasmtime's JIT works at all on the
target API levels. If it does not, the fallback is to gate `kitty-wasm` off on
Android rather than leave the model tools that cannot work — record that here
as a tombstone if it comes to it.

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
| `keyring` (`windows-native` only) | `config/providers/` | ~~Left alone — it compiles.~~ **Done in Phase 7:** moved to the `cfg(not(android))` target table and replaced on Android by the Keystore-backed store (D24). Leaving it compiling was the whole problem — see D24. |
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
| `manager.rs` | Resident slot manager (§4.1). `load/unload/status` + pressure eviction. **Hot-swap queuing is not implemented** — Phase 4.5 (§6.4) is where it becomes true. |
| `provider.rs` | `LocalProvider: Provider` (base chat trait) — streaming + reasoning, text **and** compaction. |
| `embeddings.rs` | `LocalEmbed`, on **both** platforms (D4 revised). Two entrances to one model: `POST /api/embeddings` for out-of-process callers, and `pathway_embed.rs`'s `SemanticEmbedder` for adaptive-pathway, which is linked into this binary and must not call its own socket. Hash-space remains the fallback when no embed GGUF is configured. |
| `summarizer.rs` | Summarizer chain (§4.3). Grammar-constrained JSON decode. |
| `health.rs` | `/api/health` fields (`local` state, `model_backend`, `reload_required`, `restart_pending`) + `/api/local/models/status`. |

### 3.2 Config (in the daemon, `config.rs` + env-preserving)

```
[local]
enabled = false            # flipped on by the host when a GGUF is present
model_path = ""            # absolute path, resolved host-side (§5)
embed_model_path = ""
embed_pooling = "last"     # last | mean | cls  (§9.2)
n_ctx = 4096
embed_n_ctx = 512
n_batch = 512
n_threads = 0              # 0 = let llama.cpp pick
n_gpu_layers = -1          # -1 = all layers; 0 = CPU-only
cache_type_k = "f16"       # f16 | q8_0 | q4_0  — NOT YET APPLIED, see below
cache_type_v = "f16"
```
This is the real `LocalEngineConfig`, not the sketch it replaced: paths rather
than a `default_model` id (the daemon has no idea where a host keeps models,
so Kitty resolves and passes absolute paths), `n_gpu_layers` as an `i32`
rather than the string `"auto"`, and no `backend` key yet — D20's selection
lands in Phase 4.

**Every key is reachable only through `BIGTINY_LOCAL__<KEY>` env vars.** No
host passes `--config`, so `bin/bigtiny_daemon.rs::apply_env_overrides` and
`src-tauri/src/lifecycle/bigtiny_proc.rs::spawn` are two halves of one
contract and must change together.

**Known gap:** `cache_type_k`/`cache_type_v` are accepted and ignored —
`engine.rs::base_params` never applies them. Phase 4 either wires them up or
deletes them; shipping a setting that does nothing is worse than either.
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
2. Rank devices on two keys, in order — **implemented in
   `local::backend::gpu_rank`**:
   1. backend: CUDA over Vulkan;
   2. **discrete over integrated**, then most free memory among genuine
      equals.
   Key 2 is not cosmetic and was added after measurement. An integrated GPU's
   memory *is* system RAM, so it advertises more free than a discrete card
   nearly always: on the dev laptop an Intel UHD iGPU reported **7,389 MiB
   free** next to a GTX 1650 Ti's **3,561 MiB** of real VRAM, and ranking on
   free memory alone picked the iGPU — much the slower of the two. Free memory
   is only a meaningful comparison *within* a device kind.
3. `n_gpu_layers=-1`/`auto` → hand the decision to `fit_params` (below). A
   non-negative value pins that many layers and skips fitting entirely.
4. VRAM for the card and the "Recommended for this device" badge comes from the
   same `memory_free`/`memory_total` on the device record the loader will use,
   so the two can never disagree. CPU reports `0`/`0` and the UI renders no
   budget rather than substituting system RAM.
5. An explicitly pinned backend that isn't present **falls back to CPU rather
   than erroring** — a machine that lost its GPU should degrade to slow, not to
   broken.

#### Fit formula — **superseded, do not implement**

This section specified a hand-rolled
`(file_size x resident_layer_fraction) + KV + scratch, x1.18` estimate. Don't
build it: `llama-cpp-2` v0.1.154 already exposes both halves upstream, and an
estimate that disagrees with the loader is worse than no estimate.

- `list_llama_ggml_backend_devices()` — per-device name, backend, type and
  **`memory_free`/`memory_total`**. That is D20 step 1, in
  `local::backend`.
- `fit_params()` — llama.cpp's own optimal-`n_gpu_layers` solver, wired in
  `local::engine::fit_to_device`. Three things learned wiring it:
  - It requires `n_gpu_layers` left at its `-1` default — it only writes fields
    still holding their default, so calling `with_n_gpu_layers` first makes it
    a silent no-op for the one field it exists to decide. `load` therefore
    branches three ways, and the fitting branch is the one that sets nothing.
  - It is **not thread-safe** (it mutates global llama logger state), so it
    runs under its own mutex.
  - **A returned `n_ctx` of `0` means "no opinion", not a size.** Fitting only
    rewrites `n_ctx` when it needs to shrink the context; otherwise it hands
    back the `0` it was given. Measured on a GTX 1650 Ti where all 17 layers
    fit. Passing that through reaches llama.cpp as "use the full
    `n_ctx_train`" — 128k on LFM2.5 — which is the opposite of a memory-aware
    choice, so it is normalised to `None` and falls back to 4096.

  Likewise `n_gpu_layers` legitimately stays at `-1` after a successful fit
  when everything fits; that is "all layers", not a missing value, and the
  status payload and model card both render it as such.

**Windows builds with `vulkan`** (`cuda` stays off — Vulkan covers NVIDIA, AMD
and Intel from one build, including the integrated GPUs that are the only
accelerator on most laptops, where CUDA covers one vendor and adds a ~3 GB
toolkit). Verified end to end on a GTX 1650 Ti: all 17 LFM2.5 layers offloaded.
Android stays CPU-only for v1; `local::backend` needs no change to enable it.

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
- ~~**Ollama registry**~~ — **out of scope.** Cut with managed Ollama: the
  manifest walk, blob concatenation, gzip-layer sniffing and
  compressed-vs-decoded digest rules were the larger half of this section, and
  every model in §9 is on HuggingFace. Re-add only if a wanted model is
  registry-only.
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

### 8.1 Windows build (Phase 6a) — **DONE**
- Vite multi-page: **`['overlay','hub','screenshot-select']`**. The `main`,
  `settings` and `wizard` window labels are gone; `main` became `hub`, the
  other two became routes inside it.
  - **`screenshot-select` stays a window**, contrary to this section's original
    claim that it folds into the hub as an in-window route. It cannot: it is a
    decorationless, transparent, always-on-top window positioned and sized to a
    specific monitor's bounds (`windows.rs::create_screenshot_select_window`).
    That is the same reason the overlay isn't a route.
- `src/windows/hub/App.tsx` routes `chat | settings | wizard` via
  `src/stores/routeStore.ts`. **No `sessions` route on desktop** — the session
  list is a sidebar inside the chat route, and adding a variant nothing
  navigates to would just be an unrenderable state. It arrives in 6b, where the
  mobile tab bar actually needs it.
- **Routing away from chat is safe, and this is what the whole restructure
  rests on.** `chatStore.bindEvents` subscribes to `chat://*` once per window,
  in the store rather than in a component, so a reply started in the chat route
  keeps streaming while the user is in Settings. The chat route is additionally
  kept mounted (toggled with `hidden`) so scroll position and composer drafts
  survive the trip, which store state alone would not preserve.
- Model choice lives **only** in the new-chat composer (D2).
- **Multi-window (D21):** already existed before this phase — `windows.rs`
  allocates `chat-N` labels off the same bundle, each with its own session.
  What 6a added is that the *route* is per-window too: `route_to` picks one
  live hub (preferring the focused one), stores the target under **that
  label**, and uses `emit_to` rather than a broadcast. A global target or a
  broadcast `emit` would send every open hub to Settings at once.
- `settings_target` + `wizard_mode` (per-window-kind, from when Settings and
  the wizard were windows) collapse into one `route_targets: HashMap<label,
  Value>`. `get_route_target` **consumes on read**, or a reload would bounce
  the window back to Settings long after the user left.
- Two things that were the window manager's job became the frontend's, because
  a route has no chrome to close: Settings grew a "Back to chat" control, and
  `DoneStep` navigates on finish (`complete_setup` used to hide the wizard
  window).

### 8.2 Android shell (Phase 6b) — **web layer done; native pieces are Phase 7**
- Same `<HubApp>`; bottom tabs, CSS `<480px` + safe-area.
- **Tabs are Chat / Chats / Settings**, not "Models". Models is a *section*
  inside Settings (`local_models`), and promoting it to a peer of the whole of
  Settings would have made it the only settings page reachable without opening
  Settings. The session list needed the slot more: it is a sidebar on desktop
  and has nowhere to live on a phone, which is why `sessions` became a route
  here rather than in 6a.
- The tab bar renders on every platform and is hidden by CSS above 480px,
  rather than branched on in JS — one component tree is the point of the hub,
  and a narrow desktop window then behaves the way a phone does instead of
  exercising a path only phones reach.
- `src/lib/platform.ts` stamps `data-platform` on `<html>` *before first
  paint*, from the user-agent rather than an IPC call: safe-area insets and
  the mobile shell are gated on it, and an await would flash the desktop
  layout on a phone.
- **`navTo(view)` did not need to be an IPC call.** Routing is client-side in
  `routeStore`; the only navigation Rust initiates is `open_settings` /
  `open_wizard`, which already emit `route://goto`. Adding a command that
  round-tripped to Rust only to come back would be strictly worse.
- **`shareText` and the incoming share intent are deferred to Phase 7.** Both
  need a JNI bridge and a manifest `intent-filter`; neither is a web-layer
  concern, and stubbing them here would have meant a command that silently
  does nothing on the only platform that has the feature.
- No local-model composer on Android (D18): the tab shows the single summarizer
  card (read-only) + cloud providers for chat.

### 8.3 Wizard (D14)
- One shared, in-page step flow. Adapter:
  - Windows: daemon present? → autostart → done.
  - Android: **permissions step** (POST_NOTIFICATIONS, foreground service) — only
    platform divergence.

### 8.4 Tokens / theme / safe-area / fonts (D15–D17) — **DONE**
- Type ramp `--fs-2xs` … `--fs-4xl` in `base.css`'s `:root`, all in `rem`.
  **68 px font-sizes swept, not ~110** — that estimate counted every
  `font-size` including ones already inherited.
  - The ramp keeps two half-steps (`--fs-xs-plus` = 11.5px, `--fs-sm-plus` =
    12.5px) because both are in real use, 12.5 in eight rules. Collapsing them
    into neighbours would have silently restyled the app; a rem sweep should
    change *what scales*, not what things look like. Merging them is a design
    decision for its own commit.
  - Spacing and radii stay in px on purpose. Scaling padding with the font
    would move the layout under the user when they only asked for bigger text.
- `system` theme mode (D16) resolves to `default`/`dark` from
  `prefers-color-scheme` — not a third stylesheet, so user themes are
  untouched. Follows the OS live via a `matchMedia` listener, and the
  synchronous anti-flash injection now picks the dark built-in for a dark-mode
  user instead of always flashing white.
- Safe-area: `viewport-fit=cover` on the hub page (without it `env()` resolves
  to 0 and the UI hides under the notch) plus `--safe-*` tokens that are `0`
  everywhere and only overridden under `[data-platform='android']`. Gated on
  the platform attribute, not a width media query — a narrow desktop window is
  still not a device with a notch.
- **`--font-scale` at `:root`**, consumed by `html { font-size: calc(16px *
  var(--font-scale)) }`, so it reaches every `rem`. 16px is the browser default
  it replaces, which makes an unscaled build render identically to the
  pre-sweep px values. No UI writes to it yet — Android WebView already applies
  the system font scale itself, so the desktop zoom control is the first real
  consumer and lands with whoever needs it.

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

- **`Qwen3-Embedding-0.6B` — shipped as official `Q8_0`.**
  `Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf`,
  **639,150,592 bytes** (609 MiB). **1024-dim**, `n_ctx_train` 32,768, 28
  layers. One model on Windows and Android, so beliefs embedded on one
  platform stay comparable on the other.
  - **The q4_k_m this used to name was never shippable, and the bug was the
    *pairing*.** Phase 1 validated a 376 MiB q4_k_m from the community repo
    `Mungert/Qwen3-Embedding-0.6B-GGUF`, but `curated_models.ts` pinned that
    *filename* against the **official Qwen** repo — which publishes only
    `Q8_0` and `f16`. Every embedding download 404'd, and nothing noticed
    because the failure degrades silently to hash-space recall (D4) rather
    than erroring. Verified against the live HF API, not inferred.
  - Resolved toward the official repo rather than the community one: this is
    a bundled default, so provenance shouldn't rest on a single uploader, and
    +233 MB is a fair price. Going below Q8 on an *embedder* is also the trade
    that costs retrieval quality, which is the only reason the model is here.
  - `config::UNAVAILABLE_EMBEDDING_GGUF` migrates installs already carrying
    the dead q4 id — the field is persisted, so changing the default alone
    would strand them on a file that cannot be downloaded.
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
- **Source, settled:** Qwen's official GGUF repo publishes exactly two files,
  `Q8_0` and `f16` — no smaller quant, and the community q4_k_m this section
  used to recommend is what produced the 404 above. `Q8_0` is now the pin.
  `f16` is ~1.2 GB for no recall benefit.
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

#### 2a status (2026-08-09)

Exercise any of this with `python tools/local_engine_lab.py --build` — it
builds the daemon with `--features local-engine`, spawns it against a
throwaway data dir, runs the battery and tears down. `--ab` adds an Ollama
comparison.

| | State |
|---|---|
| `LocalEngine`, `SlotManager` | Done, 262 lib tests. |
| `POST /api/embeddings` | **Working end-to-end through the daemon.** 1024-dim, unit-norm, semantically ordered (0.7325 vs 0.2936), deterministic, 400 on a bad request. |
| `register_local` at startup | Done. Registers under id `"local"`; a session pins it via `POST /api/chat/`'s `provider` field, so no DB row is needed (the `provider_type` CHECK would block one). |
| **Chat through the agent loop** | **Working end-to-end.** 15/15 harness checks pass; ~18 tok/s generating on CPU with LFM2.5-1.2B-Q4_K_M. |
| `LocalSummarizer` wiring | **Done (Phase 4.1).** `agent::summarizer_chain::SummarizerChain`: local first, then `fallback = "session_model"` through the same `ProviderRouter` every chat turn uses — no Ollama-specific client anywhere in the chain. The old Ollama-native `SummarizerClient`/`SummarizerError` are deleted outright, not deprecated. |

**First A/B (2026-08-09, Windows CPU).** In-process llama.cpp **18.4 tok/s**
generating vs Ollama **18.1 tok/s** — parity, which is the expected result
given Ollama is llama.cpp behind an HTTP server, and is the answer Phase 2b
needed. Caveat recorded by the harness itself: the two were serving *different
models* (LFM2.5-1.2B-Q4_K_M vs `qwen3.5:0.8b`), so this compares stacks, not
purely engines. Compare *generating* throughput, not wall-clock: our figure
includes a cold GGUF load the harness warms away on Ollama's side.

**Two bugs closed getting there**, both of which predate the local engine and
affect every provider:

1. **Every SSE response deadlocked at end-of-turn** (`agent::run_turn`). The
   disconnect watcher held a clone of the event sender until the *receiver* was
   dropped — but axum only drops the receiver when the response body ends, and
   the body only ends when every sender is gone. Circular: a client reading to
   EOF hung forever, and each turn leaked a watcher task. It went unnoticed
   because Kitty's frontend stops at `llm_stop` and hangs up, which breaks the
   cycle from the outside. The watcher now also races a `turn_done` oneshot.
   Regression test: `agent::tests::the_event_channel_closes_when_the_turn_ends`
   (verified to fail against the old code).
2. **Prompt prefill ignored `n_batch`** (`local::provider::generate_blocking`).
   The whole prompt went in as one `LlamaBatch`, which works only while it fits
   under the context's `n_batch` (512 by default) — an agent turn's prompt is
   routinely several times that. Now chunked, with `n_cur` tracking the
   absolute position rather than the last chunk's length.

One diagnostic hazard remains, worth fixing regardless: `Delta::error_type` is
**never read** by `process_stream` (the dead field from the 88bugs re-audit,
#62), so a provider-side failure signalled only that way produces silence.
`generate_blocking` now also `tracing::error!`s and emits the message as
`content` so it can't vanish. Separately, a session pinned to an
**unregistered** provider still hangs rather than erroring.
#### 2b + 3 status (2026-08-09) — **DONE**

Landed as one branch, so the first-run wizard was never without a local path.

| | |
|---|---|
| AP embeddings | **In-process**, not over HTTP. See the correction below. |
| Managed Ollama | Gone: ~700 LOC of process/pull/env plumbing deleted. |
| `provider_type: "ollama"` | Kept and working, as a *remote* endpoint. Pinned by `a_legacy_ollama_profile_still_routes_after_managed_ollama_was_removed`. |
| Downloader | HuggingFace only, resumable, sha256-verified. Ollama registry cut. |
| Settings → Local Models | Replaces the Ollama tab. |
| Wizard | fork → download → configure → memory model → done. No "Detect" step. |
| `local` provider type | **New.** Makes the engine selectable; without it the downloaded model had nothing to load it. |
| Shipped daemon | Built `--features local-engine`. **+3.1 MB**, not the +50–100 MB assumed. |

**The spec was wrong about the AP re-point.** It said to flip
`AP_EMBED_OLLAMA_URL` at the daemon's own `/api/embeddings`. That is the
daemon issuing an HTTP request to its own listener — a socket round-trip and a
second copy of every vector to reach a slot manager one struct field away,
which also has to satisfy the API-key middleware D25 makes mandatory on
Android. Instead `adaptive_pathway::embed::SemanticEmbedder` is a host-supplied
hook, implemented over the shared `SlotManager`. The HTTP route stays for
out-of-process callers and for anyone pointing at a real Ollama.

**The space tag is load-bearing.** `cfg.embedding.ollama_model` is compared,
not displayed: `list_recall_candidates` filters on it and
`sync_embedding_model_fingerprint` diffs it against disk. It is now derived
from the GGUF filename (`local:<stem>`), so swapping weights correctly marks
old beliefs stale for `reembed_stale_beliefs` rather than silently comparing
vectors across two incompatible spaces.

**Config migration.** `summarizer.model` and `adaptive_pathway_embedding_model`
held Ollama tags and now hold GGUF ids. `migrate_model_tags_to_gguf` rewrites
only the exact tags Kitty itself wrote — a hand-typed value may name a model
the user fetched themselves. Without it both engine slots would go silently
unconfigured on every existing install.

**Two bugs fixed in passing**, neither related to the local engine: every SSE
response deadlocked at end-of-turn (`agent::run_turn`'s disconnect watcher
held a sender the response body was waiting on), and prompt prefill ignored
`n_batch`.

### Phase 3 — Downloader — **DONE** (landed with 2b, above)
- **HuggingFace only.** The Ollama-registry source (manifest walk, blob
  concat, gzip layers, compressed-vs-decoded digest rules) was cut with
  managed Ollama — every model in §9 resolves through an HF `resolve` URL, and
  `flate2` is not a dependency.
- **Acceptance, met offline:** resume-after-kill, sha256-mismatch
  retry-then-fail, atomic rename, 1.5× refuse gate — plus two the spec didn't
  ask for and the implementation needed: a `.part` whose `.meta` sidecar names
  a different source is discarded rather than resumed, and a `.part` with no
  sidecar is discarded. Resuming across either boundary burns a full download
  before failing the checksum, with nothing pointing at why.
- **Still owed (Phase 7):** the Android byte-offset-resume-on-connectivity-drop
  test, which needs the foreground service to exist first.

### Phase 4 — Settings
- **4.3 GPU is live.** `src/local/backend.rs` implements D20's device query,
  ranking, explicit pinning, VRAM reporting and CPU fallback over llama.cpp's
  own registry — 12 unit tests, none needing a GPU, since the policy is
  testable against a synthetic device list. `src/local/engine.rs`'s
  `fit_to_device` implements the fit half. Validated on a GTX 1650 Ti with
  the `vulkan` cargo feature on: 17/17 LFM2.5 layers offloaded.

  Three findings the design didn't anticipate, all in §3.3:
  - **Discrete must outrank integrated**, ahead of free memory. An iGPU
    reports system RAM as its budget and so nearly always claims more than a
    discrete card — 7,389 MiB vs 3,561 MiB on the dev laptop — and the
    original ranking picked the slower device.
  - **A fitted `n_ctx` of `0` means "no opinion"**, not a size, and must not
    reach llama.cpp (it reads it as the full 128k `n_ctx_train`).
  - **`n_gpu_layers` stays at `-1` after a successful fit** when everything
    fits. That is "all layers", not a missing value.

  Enabling `vulkan` also turned up two Windows build traps (nested-project
  cmake generator, and `MAX_PATH`) — both recorded in §11.
- Knobs/presets/model card/badge/health; **auto-restart scheduling** (§6.4);
  backend-aware hiding. Acceptance: settings round-trip via `commands/` + UI;
  **restart applies immediately when idle, and only after the in-flight
  generation completes when busy** (verified with a long-running stream); no
  silent load-param failures.

### Phase 5 — Scheduled tasks overrides — **DONE**
- `ScheduledTask.model_id: Option<String>` (`#[serde(default)]`, so tasks
  written before it still load meaning "use whatever is active" — the
  behaviour they had). A *model* id, not a provider id: a provider id would go
  stale the moment a profile is deleted and recreated.
- Applied in `lifecycle::scheduler::fire_scheduled_task` via the existing
  `set_session_provider`, *before* the prompt is sent so the first turn runs
  on it. Deliberately not passed to `new_session`: `POST /api/chat/` only
  honours `model` when `provider` is sent alongside it (`routes/chat.rs`), and
  an override names a model while the provider is whichever is active at fire
  time — the `PATCH` path pairs them correctly.
- Picker in `ScheduledTasks.tsx`, listing downloaded GGUFs with "whatever is
  active when it runs" as the default.

### Phase 6a — Desktop hub — **DONE**
- Window components folded into `hub`; Vite intake down to three entries;
  `routeStore`; per-window route targeting. Full detail in §8.1, including the
  two places the original plan didn't survive contact with the code
  (`screenshot-select` can't be a route; `sessions` has nothing to route to on
  desktop yet).
- **Multi-instance (D21) was already there** — `chat-N` labels predate this
  phase. 6a made the *route* per-instance to match.
- Automated: `tsc`/`eslint`/`vitest` (249) and `cargo test`/`clippy` green;
  `routeStore.test.ts` covers payload parsing and deep-link retention.
- **Still owed — manual, not yet run:** two hub windows on two sessions with
  two pinned models generating concurrently; a "Fix this" deep link from a
  degraded overlay; first-run wizard through to chat.

### Phase 6b — Android shell + tokens + scale — **web layer done**
- Mobile shell, tokens/theme `system`, safe-area, px→rem sweep: all landed.
  See §8.2/§8.4 for the four places the plan changed (tab set, `navTo`,
  `shareText`, the sweep's real size).
- **Deferred to Phase 7, correctly:** `shareText` and the incoming share
  intent. Both are JNI + manifest work, not web-layer work.
- **Breakpoint verified in a real engine at 375x812 and 1280x800**, by serving
  `base.css` and measuring computed styles. It caught a bug that no test would
  have: the mobile block was authored *above* the desktop rules it overrides,
  and since every override is the same specificity, source order won — the
  phone layout kept a 260px grid track for a sidebar that was already
  `display: none`, squeezing chat into 115px of a 375px screen. The block is
  now last in the file, with a comment saying why it has to be.
  - Confirmed at 375px: single-column chat at full width, sidebar and
    artifacts hidden, 3-up tab bar pinned to the bottom at 44px, no horizontal
    overflow. At 1280px: `260px 740px 280px` and no tab bar — unchanged.
- **Still owed:** on-device rendering. The above used a desktop browser at a
  phone viewport, which exercises the CSS but not the WebView, real safe-area
  insets (`env()` resolved to 0 with no notch to report), or system font
  scale.

### Phase 7 — Android native — **IN PROGRESS (scaffold up, app launches)**

**Done:** `tauri android init` run; a debug APK builds, installs, and starts
`MainActivity` on a Pixel 10 Pro (Android 16 / API 36 / arm64-v8a / 16 GB).
Two real bugs fixed on the way, neither reachable from `cargo check` (see
commit `abe9ac9`):
- `run()` had no `#[tauri::mobile_entry_point]`, so the `.so` exported **zero**
  dynamic symbols and the app died on launch with `UnsatisfiedLinkError`.
- The `.so` linked at 4 KB pages; Android 16 raised
  `PageSizeMismatchDialog`. Fixed with `max-page-size=16384` link args.

**Toolchain notes for whoever picks this up:**
- Prereqs installed this session: OpenJDK 21 (`Microsoft.OpenJDK.21`),
  `tauri-cli` 2.11.4. SDK already had platform-34, build-tools 34, NDK 27.2;
  Gradle self-provisioned what compileSdk 36 needed, and `cmdline-tools` was
  never required.
- **`cargo tauri android build` needs Windows Developer Mode**, because it
  symlinks the `.so` into `jniLibs`. Enabling it is not enough for an
  already-running shell — `SeCreateSymbolicLinkPrivilege` is granted at logon,
  so the terminal must be restarted. Workaround that needs neither: build the
  lib, copy it to `app/src/main/jniLibs/arm64-v8a/`, then
  `gradlew assembleArm64Debug -x app:rustBuildArm64Debug`.
- `src-tauri/gen/` is gitignored, so the generated project is not tracked.
  Whether it should be is a Phase 8 call.
- `tauri.android.conf.json` already zeroed `externalBin`/`resources` back in
  Phase 1a — the sidecar problem §2.3 describes was solved before this phase.

**Landed 2026-08-11** — the two release blockers, both via one Tauri Android
plugin (`KittyPlugin`, in the app Gradle module rather than a separate library:
it is one app's glue, and `find_class` resolves it through the activity's
classloader either way). Rust side: `src-tauri/src/android/`.

- **`SecretStore` (D24).** Not keyring's Android backend — **there isn't one**;
  see the revised D24. `SecretStore.kt` seals values with AES-256-GCM under a
  non-exportable AndroidKeyStore key and stores the blobs in private
  SharedPreferences (`commit`, not `apply` — a secret still in the prefs cache
  when the process dies is the bug being fixed). `config::providers::keyring`
  dispatches per platform, and the absent-vs-unreadable distinction that
  `classify_read_result` protects is preserved across the boundary: Kotlin
  resolves `{found: false}` for absent and *rejects* for unreadable.
  `keyring` is out of the Android dependency graph entirely, so the mock
  cannot silently return.
- **Download-while-backgrounded.** `DownloadService` — `dataSync` foreground
  service, `START_STICKY`, low-importance progress notification, POST_
  NOTIFICATIONS requested at the first download rather than at startup, and a
  6-hour-capped partial wake lock (capped so a missed `stop` expires instead of
  draining the battery until reboot). A RAII `foreground::Session` in
  `commands::models` starts it and stops it on drop, so every early-return
  failure path tears it down.
  **No `ConnectivityManager.NetworkCallback`**, contrary to the original plan:
  the socket dies on a Wi-Fi↔cellular handoff whatever we observe, and the
  downloader already resumes from the `.part` offset with a `Range` header — so
  a bounded, *progress-aware* retry loop (budget resets whenever bytes
  advanced) covers the handoff, a tunnel, and a flaky AP with one mechanism
  instead of three.

**Not started:** everything below.

- GGUF first-use path, no local chat picker, backward-compat.
- **Daemon hardening (D25, §2.6):** `require_secret: true` with a generated
  secret; relaxed escalation-to-approval for the sandboxed data root.
- **Tool surface:** apply §2.4's `lean_analyze_workspace` scoping;
  `lean_shell` is already excluded, leave it that way.
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

### Phase 8 — Packaging — **DONE**

**Landed 2026-08-11.**

- **No Android sidecars, structurally.** `plugins/build.py` was already
  Windows-triple-only and produces `.exe`s; its docstring now says *why* that
  is correct rather than incidental, and says not to add an Android triple.
  `tauri.android.conf.json` clears `bundle.externalBin`/`resources` (done in
  Phase 1a, kept). The daemon is linked in and hosted in-process; `kitty-tools`
  / `kitty-web` / `kitty-wasm` register with `transport: "in_process"`; the
  behavioral-memory engine comes along inside the daemon crate.
- **Signing.** `app/build.gradle.kts` reads a gitignored
  `gen/android/keystore.properties` and applies it to the release variant.
  **The key is deliberately not in this repo** — absent the file, the release
  variant builds *unsigned*, which still verifies the lane and which Play
  rejects, so nothing ships by accident. Creation steps in `docs/RELEASE.md`.
- **`minSdk` 24 → 26.** The native library is compiled against the API 26
  sysroot (`cargo ndk --platform 26`), so on 24/25 the APK installs and then
  dies at `System.loadLibrary`. Declaring 26 converts a runtime crash into
  Play refusing the install.
- **`src-tauri/gen/android/` is now tracked** (`gen/schemas/` still ignored).
  It is not regenerable: it carries the manifest's `windowSoftInputMode`, the
  `minSdk` bump and the signing config. Its own `.gitignore` excludes the build
  tree, the staged cdylib, and the 26 MB wasm resource copy.
- **Docs.** `AGENTS.md` gained an Android lane (including the trap that
  `cargo check --target aarch64-linux-android` is *not* a substitute for
  `cargo ndk`, because it skips `llama-cpp-sys-2`'s build script entirely and
  passes on code that does not build). `docs/RELEASE.md` split into Windows and
  Android sections with the full toolchain env. `CLAUDE.md`, `README.md` and
  `docs/ARCHITECTURE.md` rewritten off "Windows-only" and off Ollama;
  `docs/VERSIONS.md`'s Ollama section and installer URL marked historical;
  `docs/ADAPTIVE_PATHWAY.md` banner-superseded (it documents the retired Python
  sidecar and its `AP_EMBED_OLLAMA_*` config).

Both former release blockers — D24's keyring mock and the missing download
foreground service — were closed on the same day, in Phase 7 above. What is
left is smaller: the CPython WASI guest still downloads on first use
(`docs/BACKLOG.md`), and the Phase 7 "not started" list still has the daemon
hardening and tool-surface scoping items.

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
  - **Vulkan SDK**, required by the `vulkan` cargo feature (`glslc`, to
    compile the shaders). `winget install KhronosGroup.VulkanSDK`. Same
    failure shape as libclang: a cmake-step error that reads like a crate bug.
    Enabling it brought two *further* traps, both Windows-only and both
    fatal-on-first-build:
    - **`CMAKE_GENERATOR=Ninja` is mandatory here too**, not just for Android.
      The Vulkan shader generator is a nested `ExternalProject` that runs its
      own cmake configure and inherits only the environment, not the outer
      generator's toolset. Under the default Visual Studio generator that
      nested configure reports `No CMAKE_C_COMPILER could be found` and the
      build dies with `The system cannot find the batch label specified -
      VCEnd`. Build from a vcvars64 shell so `cl.exe` is on `PATH`.
    - **`MAX_PATH`.** That nested project's try-compile scratch directory adds
      ~140 characters, which pushes past 260 under any moderately deep repo
      root (`.../OneDrive/Documents/Coding Projects/Kitty/Kitty/...` is
      enough). `cl.exe` still enforces `MAX_PATH` even with the OS's
      `LongPathsEnabled=1`, and fails with `Cannot open source file:
      ...testCCompiler.c: No such file or directory` — which reads like a
      cmake bug, not a path-length limit. Set `CARGO_TARGET_DIR` to something
      short (`C:\kt`).
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