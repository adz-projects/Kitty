# Kitty Android & Local-Inference — Execution Plan

Status: **Execution plan. Approved.** Grounded in `docs/ANDROID.md` (the spec) against
the current repo state. Do the phases in order — Phases 1a/1 are the hard upstream gate.

## Current-repo reality check (verified — re-checked 2026-08-08)

- `plugins/bigtiny_rust/` exists, builds a `bigtiny-daemon` bin with chat/compaction/
  provider/MCP already — but **Ollama is still wired everywhere** and no llama.cpp
  binding is **a dependency yet** (repo-wide grep for `llama` finds only "ollama"
  substring hits and this plan's own text).
- Daemon currently spawns the frozen Python `bigtiny-daemon` (`src-tauri/src/lifecycle/bigtiny_proc.rs`);
  the Rust daemon is the replacement target.
- Windows windows: `main/overlay/settings/wizard/screenshot-select` — **no `hub`** yet;
  `vite.config.ts` is multi-page.
- No Android config / NDK in `tauri.conf.json`; `externalBin` lists only Windows sidecars.
  `src-tauri/gen/` holds only `schemas/` — **`gen/android` has never been generated**
  (`tauri android init` has not been run).
- **The in-process MCP transport already exists and already hosts four servers** —
  `kitty-tools`, `kitty-web`, `kitty-wasm`, **and** `pathway`, all as linked path-deps
  with `serve_in_process` entry points (`ANDROID.md` §2.3 has the file map). Phases 1/7/8
  scope accordingly: **cross-compile + re-register**, not build-from-scratch.
- ~~Desktop-only deps are not gated yet~~ — **done 2026-08-08.** `src-tauri`
  cross-compiles clean for `aarch64-linux-android` (zero warnings), desktop
  unregressed. Full record in `ANDROID.md` §2.5. `keyring` was left as-is: it
  compiles via keyring 3.x's mock fallback, which is a *silent secret-loss*
  problem for Phase 7 (D24), not a build one.
- `reqwest` is already on `rustls-tls` workspace-wide (no `native-tls`/`openssl` in
  the lockfile) — the OpenSSL cross-compile problem is **already avoided**. Don't
  undo it.
- **Baseline is now clean** (the earlier in-flight `tauri.conf.json`/`Cargo.lock`/
  binaries/`docs/ANDROID.md` changes have since been committed). `git status` shows
  only the untracked `opencode/` dir and a stray `package-lock.json` — both unrelated
  to this work. Phase 0's exit condition is effectively already met; re-verify, don't
  redo.

## Toolchain / environment facts (verified — re-checked 2026-08-08)

- No JDK, no Android SDK/NDK, no `cargo-ndk`, no `aarch64-linux-android` rust target,
  no `adb`, no `cmake` on PATH. Only `x86_64-pc-windows-msvc` rust target. **(Still
  true — nothing in Phase 1a has been started.)**
- Host is x86_64 Windows → **cannot run an arm64 AVD**; physical arm64 device is the
  ARM-accurate gate coverage (user confirmed a device is available).
- **172.6 GB free disk** (was 94.5 GB at first check — ample either way).
  `winget` available (no choco). **Rust/cargo 1.97.1** (was 1.96.1).
- **Decisions locked:** CLI-only toolchain (no Android Studio GUI, no AVD image by
  default); JDK 17 pinned; physical arm64 device = ARM gate.

---

## Phase 0 — Stabilize baseline

**Largely already satisfied** — the in-flight work this phase was written for has
since been committed. Re-verify rather than redo:

- `git status` → should show only the untracked `opencode/` and `package-lock.json`
  (both unrelated; neither blocks anything). Commit/stash anything else first.
- `pnpm test` and `pnpm lint`
- `cargo test` + `cargo clippy` in `src-tauri/`, `plugins/bigtiny_rust/`, and
  `plugins/adaptive-pathway_rust/`

**Exit:** a clean, reproducible green baseline.

---

## Phase 1a — Android dev environment bootstrap (CLI) — **DONE (2026-08-08)**

What actually worked (the original steps 2–3 were wrong — recorded so a rebuild
on another machine doesn't repeat the detour):

1. **JDK 17** — `winget install --id Microsoft.OpenJDK.17 --exact --silent`.
   Lands at `C:\Program Files\Microsoft\jdk-17.0.20.8-hotspot`; set `JAVA_HOME`.
2. **Android CLI** — `winget install --id Google.AndroidCLI --exact --silent`.
   **`Google.AndroidSDK` does not exist as a winget package.** What this
   installs is Google's *newer unified* `android` CLI, not the classic
   `cmdline-tools`/`sdkmanager` — so there is **no `sdkmanager --licenses`
   step** (it writes `Sdk\licenses` itself on first install). It bootstraps
   the SDK at `%LOCALAPPDATA%\Android\Sdk` on first run.
3. **SDK components** — `android sdk install <pkg>`, where package IDs are
   **slash-separated**, not the classic `;` form, and a bare `ndk` is rejected:
   ```
   android sdk install platform-tools
   android sdk install platforms/android-34 build-tools/34.0.0
   android sdk install ndk/27.2.12479018
   ```
   Two gotchas: `android sdk list` only lists *installed* packages (the
   available set lives in the protobuf blobs under `Sdk\.sdk\objects`), and the
   CLI **exits 9 even on success** — verify by checking the directories, not
   the exit code.
4. **CMake** — `winget install --id Kitware.CMake --exact --silent`.
5. **Rust android target + cargo-ndk** — `rustup target add aarch64-linux-android`;
   `cargo install cargo-ndk` (got 4.1.2).
6. **Env vars** (user scope): `JAVA_HOME`, `ANDROID_HOME`, `ANDROID_SDK_ROOT`,
   `ANDROID_NDK_HOME`, `NDK_HOME`; `Sdk\platform-tools` appended to `PATH`.
7. **adb + device wiring** — `adb` works; **no device attached yet**. Enable USB
   debugging and confirm `adb devices` before Phase 1's on-device acceptance.
8. **`src-tauri` Android-buildability pre-flight** — **done**; the full record
   of what was gated is `ANDROID.md` §2.5. Three things worth carrying forward:
   - The list in the original plan was **incomplete**. `screenshot.rs` (a whole
     ungated Win32 GDI module) and `config/env_helper.rs` (a second `winreg`
     consumer) were the two biggest breakers and neither was listed.
   - `notify-rust` was **already** correctly gated in `Cargo.toml`; only its
     call site needed splitting. The Android arm over
     `tauri-plugin-notification` had to be *written* — the plugin was
     registered but never called from Rust.
   - `create_overlay`/`show_overlay` needed the **functions** gated, not just
     the call site: `decorations`/`always_on_top`/`skip_taskbar` don't exist on
     the mobile `WebviewWindowBuilder`.
9. **Build-script blocker** — `tauri-build` resolves `externalBin` by target
   triple and fails on `binaries/kitty-wasm-aarch64-linux-android` *before any
   Rust compiles*. Fixed with a new **`src-tauri/tauri.android.conf.json`**
   clearing `externalBin`/`resources` for Android. Note `"targets": ["aab"]` is
   **not** a valid `BundleTarget` — omit the key entirely.
10. **Toolchain self-test** — `cargo ndk -t arm64-v8a --platform 26 check` on
    `src-tauri` is clean. Note cargo-ndk 4.x uses `--platform` for the API
    level; `-p` is cargo's *package* flag and produces a confusing panic.
    Still to do when Phase 1 needs it: `pnpm tauri android init` (generates
    `gen/android`) and a real APK/AAB packaging run.

**Acceptance (1a):** **met**, except the device. `cargo ndk … check` clean;
cargo-ndk builds to arm64; JDK/CMake/adb/NDK all respond; NDK version recorded
in `ANDROID.md` D22. **`adb get-state` still has no device** — plug in the arm64
device and enable USB debugging before Phase 1's on-device acceptance. Disk used
well under the ~10–15 GB budget.

> **Aside worth knowing:** a `cargo-ndk` panic dumps the *entire* process
> environment into its report, secrets included. Don't paste one into a public
> issue without scrubbing.

---

## Phase 1 — Cross-compile spike (gate; highest risk)

- Add `wasmtime` + the llama.cpp binding (feature-gated) + Android build profile to
  `bigtiny_rust`. **`wasmtime` is needed because it backs `kitty-wasm`**, the fourth
  in-process MCP builtin (sandboxed code-execution tools over a pinned CPython 3.12
  WASI guest) — not because anything hosts Python tooling; `kitty-docs-web` is retired
  and its PDF/Excel/web tools are native Rust now.
- `cargo build --target aarch64-linux-android` for llama + wasmtime + sqlx **and all
  four linked path-dep crates** (`adaptive-pathway`, `kitty-tools`, `kitty-web`,
  `kitty-wasm`). The latter two are pure Rust and should be uneventful — they're named
  so "the daemon builds" means every linked crate builds.
- NDK linkage per `ANDROID.md` §11: `ANDROID_STL=c++_static`, static
  `libgcc`/`libunwind`/`libatomic`, **`LLAMA_OPENMP=OFF`**, single cdylib, no leaked
  JNI `.so` (if one leaks, `System.loadLibrary` in `gen/android` Kotlin before Rust init).
- Confirm `lfm2.5…q4_k_m.gguf` loads via `llama_cpp` and mints tokens; confirm `-1` =
  "all layers" across CUDA/Vulkan/CPU on the Windows host (validates D20 shape for both
  platforms).
- Windows host-side: **Vulkan runtime** (usually present) + **CUDA toolkit (~3 GB)**
  deferred to Phase 2 real GPU testing — not needed for the compile gate.
- **Gate acceptance:** boots on the physical arm64 device with zero
  `UnsatisfiedLinkError`/`dlopen` failures; `readelf -d`/`nm -u` on the produced `.so`
  shows no unresolved external symbols. Tombstone the `ANDROID.md` §9 fallback decision
  (`qwen3` / llama version) if `lfm2` fails.

> **Do not start Phase ≥2 coding until Phases 1a + 1 are green.** The whole local-engine
> story depends on the cross-compile gate.

---

## Phase 2 — Daemon local engine + Ollama removal (Windows first)

- Add `bigtiny_rust/src/local/`:
  - `engine.rs` — `LocalEngine` over `llama_cpp`; builds model/context from
    `LocalEngineConfig` (`n_ctx`, `n_batch`, `n_threads`, `cache_type_k/v`, flash_attn
    derived D19).
  - `manager.rs` — resident slot manager (§4.1): `load/unload/status`, hot-swap queueing.
  - `provider.rs` — `LocalProvider: Provider` — streaming + reasoning, text **and**
    compaction.
  - `embeddings.rs` — desktop-only `POST /api/embeddings` (D4).
  - `summarizer.rs` — summarizer chain (§4.3), grammar-constrained JSON decode.
  - `health.rs` — `/api/health` fields (`local`, `model_backend`, `reload_required`,
    `restart_pending`) + `/api/local/models/status`.
- Add `[local]` + `[summarizer]` config (§3.2); `select_backend()` (D20) + the shared
  §3.3 fit formula (×1.18 margin) driving badge/VRAM/RAM-warning; per-backend VRAM reads
  (DXGI / Vulkan heap / 0 for CPU); `flash_attn` auto-detect (D19).
- Re-point AP at the daemon (`AP_EMBED_OLLAMA_URL` → daemon port, D4); route compaction/
  summarizer through it.
- **Remove Ollama everywhere:**
  - Delete `src-tauri/ollama/`, `commands/ollama.rs`, `lifecycle/ollama_proc.rs`,
    `lifecycle/summarizer_model.rs`, `config/env_helper.rs`, `state.ollama`,
    `providers::active_ollama_target`.
  - Strip frontend `ollamaXxx` IPC + wizard Ollama steps + `StackStatusView` ollama
    states.
  - `grep -i ollama` sweep across live code.
- **Migration for existing installs:** `ProviderProfile.provider_type` is an untyped
  `String` (per the existing `old_shape_provider_migrates_with_defaults` test), so a
  saved `"ollama"` profile still deserializes after removal — it just becomes an inert,
  unreachable provider. Surface it as an explicit *removed provider* state in
  Settings → Providers with a delete/replace action (CLAUDE.md rule 6), rather than a
  silently dead row.
- **Acceptance:** `cargo test` + `cargo clippy` clean; chat + compaction run through
  `LocalEngine`; embeddings round-trip via AP; secrets via keyring; no live `ollama`
  references; **a pre-migration `config.json` carrying a `provider_type: "ollama"`
  profile loads without error and shows the removed-provider state**.

---

## Phase 3 — Model downloader (Kitty core, `commands/models.rs`)

- Commands: `list_local_models` (GGUF header size), `delete_model` (manual only, D7),
  `get_disk_free`, `download_model` (`{id, source: huggingface|ollama, url|repo, file|tag, rev?}`).
- Flow (both sources): refuse gate `free_space < size × 1.5` (D7); `.part` + `.part.meta`
  (expected total, sha256); `Range` resume; sha256 verify → mismatch deletes `.part`,
  auto-retry once, then explicit error; atomic rename → `.gguf`; `models://*` progress.
- **HuggingFace:** `resolve/{rev}/{file}`; sha256 from HF API.
- **Ollama registry:** manifest → ordered blobs → concatenated `.gguf`. Per-layer
  `Content-Type` + gzip-magic (`0x1f 0x8b`) check; decode gzip layers via streaming
  `flate2`; digest verified per stored (compressed) blob; final sha256 gate over decoded
  concatenated `.gguf`.
- **Acceptance (tests):** resume-after-kill; sha256-mismatch retry-then-fail; atomic
  rename; 1.5× refuse gate; **gzip-compressed Ollama layer fixture** (flate2 decode,
  content-digest vs decoded-GGUF verification); Android **byte-offset resume on
  connectivity drop** test.

---

## Phase 4 — Settings (`Settings → Local models`, shared component)

- Engine knobs (Windows: GPU+CPU; Android: CPU-only hidden GPU knobs, D19/D20):
  `n_ctx`, `n_gpu_layers` (auto/-1/0/N), `n_batch`, `n_threads`, `cache_type_k/v`,
  `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty`, `timeout_s`.
- Presets (D6): Precise 0.1/50/0.95/1.05, Balanced 0.6/40/0.9/1.05, Creative
  1.0/0-off/1.0/1.1.
- Model card + health dot + badge (D10/D13): size (GGUF header), RAM/VRAM (same §3.3 fit
  fn — never disagrees), backend now, flash_attn diag, health (`ready`/`download_failed`/
  `load_failed`/`slot_busy`).
- §6.4 auto-restart: load-time param change marks `reload_required` + schedules restart;
  immediate when idle, queued until the in-flight generation completes when busy;
  non-blocking "restarting…" chip; `reload_required` + `restart_pending` in `/api/health`.
- **Acceptance:** settings round-trip via `commands/` + UI; **restart applies immediately
  when idle, only after in-flight generation completes when busy** (verified with a
  long-running stream); no silent load-param failures.

---

## Phase 5 — Scheduled-task overrides (D3)

- `ScheduledTask` gains `model_id: Option<String>` (empty = summarizer default).
- Fire: pass `model_overrides` on `send` (`PATCH /api/chat/{id}/model`); else default.
- Picker: Windows lists local models; Android lists cloud providers + summarizer.

**Acceptance:** task fires with explicit model override; default path uses summarizer.

---

## Phase 6a — Desktop hub (merge + multi-instance, D21)

- Vite multi-page → `['overlay','hub']`; fold `settings`/`wizard`/`screenshot-select`
  into in-window routes inside `src/windows/hub/` (`<HubApp>` routing
  `chat | sessions | settings | wizard` via a tiny zustand `routeStore`).
- Model choice lives **only** in the new-chat composer (D2).
- **Multi-instance hub:** each hub instance = independent viewer with its own session and
  own pinned model. `open_new_chat_window`/`open_main` create a new hub instance when a
  distinct session is requested; `route://goto` emits + `focus_or_open` target a specific
  instance; `get_settings_target` returns a route payload.
- **Acceptance:** overlay + hub behave pre-restructure (regression via `pnpm test`); **two
  hub windows, two different sessions, two different pinned models generate concurrently**
  (each with its own chat slot).

---

## Phase 6b — Android shell + tokens / theme / safe-area / fonts (D15–17)

- Same `<HubApp>`; bottom tabs (**Chat / Models / Settings**), share intent, embed/back;
  CSS `<480px` + safe-area. `ipc.ts` adds `navTo(view)`, `shareText`.
- No local-model composer on Android (D18): Models tab shows the single summarizer card
  (read-only) + cloud providers for chat.
- Extend `src/themes/` contract: semantic token ramp (spacing/type/status) +
  `--safe-top/right/bottom/left`. `system` theme mode = `default` under
  `prefers-color-scheme: light`, `dark` under `dark`.
- Safe-area: `viewport-fit=cover` + `env(safe-area-inset-*)` only on Android; desktop reads
  same tokens at `0`.
- **`--font-scale` at `:root`** consumed via `rem`; Android font-scale + Windows zoom both
  write into it → shared breakpoints. **Full px→rem sweep of `base.css`** (~110 hardcoded
  font sizes) in one pass.
- **Acceptance:** appearance-parity screenshot check: overlay + hub on both OS.

---

## Phase 7 — Android native (`gen/android`)

- `KittyForegroundService` (`dataSync`, `START_STICKY`, POST_NOTIFICATIONS grant via
  wizard §8.3).
- `SecretStore` — **the `keyring` crate's Android/Keystore backend (D24), not a
  bespoke store**: its own Cargo feature **plus** JNI `JavaVM`/`Context` init wiring
  before any `keyring::Entry` call. Budget as a spike — `migrate_secrets` /
  `get_or_create_bigtiny_encryption_key` assume one backend shape and need that init
  to have already run.
- **Daemon hardening (D25):** `require_secret: true` with a generated secret — loopback
  is **not** process-private on Android, so without this every app holding `INTERNET`
  can drive the agent. Also relax escalation-to-approval where the app sandbox is
  itself the security boundary.
- **MCP registration flip (`ANDROID.md` §2.3).** In
  `src-tauri/src/bigtiny/mcp.rs::ensure_builtin_servers`, register `kitty-tools` /
  `kitty-web` / `kitty-wasm` as `transport: "in_process"` with the logical name as
  `command` — mirroring the `"pathway"` row already there — instead of
  `transport: "stdio"` + a bundled exe path. Platform-gated; desktop unchanged.
  The daemon side needs nothing: `mcp::builtin::connect` already has all four match
  arms, guarded by `every_advertised_builtin_actually_connects`.
- **Tool surface (`ANDROID.md` §2.4):** scope `lean_analyze_workspace` to app-private
  storage; `lean_shell` is already `cfg`-excluded — also gate the `ALWAYS_ON_TOOLS`
  constant in `plugins/kitty-tools/tests/protocol.rs`, which still asserts the
  22-tool desktop surface unconditionally.
- GGUF first-use download path; no local chat picker; backward-compat.
- Download-while-backgrounded: dataSync FGS + visible notification, `ConnectivityManager`
  `NetworkCallback` for Wi-Fi↔Cellular handoff, byte-offset resume (§5.2). Multi-GB GGUF
  pull survives Deep Doze and network changes without corruption.
- **Acceptance:** build + install on device; summarizer cloud fallback fires; wizard
  grants perms; OEM battery restart resumes; **a model download interrupted by airplane
  mode resumes from the same byte after reconnect**; **an unprivileged `curl` from an
  `adb shell` cannot reach `/api/*` without the secret** (the same position any other
  installed app is in).

---

## Phase 8 — Packaging & docs

- `plugins/build.py` must **not** freeze Rust sidecars for Android; `externalBin` removed/
  gated for Android; daemon + `kitty-*` linked in. Signed AAB + Windows installer.
- `AGENTS.md` gains an `android` lane; scrub OLLAMA references in `docs/`.
- **Update `CLAUDE.md`** — its "Windows-only Tauri v2 desktop app" / "Windows-only
  target" framing (and its Ollama references) stays accurate until this phase, which
  is why it's deliberately untouched earlier. Rewrite to the dual-target reality here.
  Also correct its `kitty-tools` tool count (says 18; actually 22 always-on + 3 viz).

**Acceptance:** Android AAB builds/signs with daemon linked in; Windows installer still
builds; commands table updated; `CLAUDE.md` no longer describes a Windows-only app.

---

## Per-gate commands

Set `ANDROID_NDK_HOME` first; use `cargo ndk` rather than a bare
`--target aarch64-linux-android`, or `ring`'s build script can't find the NDK
clang and fails before anything of ours compiles.

```
git status                                                     # Phase 0 re-verify (baseline already committed)
cargo test && cargo clippy                                     # src-tauri + bigtiny_rust + adaptive-pathway_rust
cargo ndk -t arm64-v8a --platform 26 check                     # in src-tauri/ — the 1a gating check (clean)
adb devices                                                    # 1a wiring check (no device yet)
cargo ndk -t arm64-v8a --platform 26 build --manifest-path plugins/bigtiny_rust/Cargo.toml   # Phase 1 gate
pnpm test && pnpm lint                                         # after each frontend phase
```

## Key risks / deferred items

- **Phase 1 (llama binding + wasmtime + sqlx on aarch64, NDK static linkage) is the
  make-or-break gate** — validate a working cdylib before any UI work.
- **wasmtime** is not currently a dependency; must be added (feature-gated) for the spike.
- ~~`winreg` breaks the build before any of that is reached~~ — **resolved**
  with the rest of the Phase 1a pre-flight. The lesson that generalizes: two of
  the worst breakers weren't on the checklist at all, and the failures read like
  toolchain problems. When Phase 2 and Phase 6b touch this surface, grep for the
  *second* consumer of anything platform-specific.
- **The Android loopback exposure fails silently** — no error, no log, all tests still
  green, daemon simply reachable by every other app on the device. Nothing surfaces it
  naturally, which is why it's an explicit Phase 7 acceptance check.
- **CUDA toolkit deferred** to Phase 2 desktop GPU testing (~3 GB, not needed for compile
  gate). Vulkan runtime on Windows host.
- **Emulator:** device-only satisfies the arm64 gate; add an x86_64 host AVD only if a
  no-hardware path is wanted (optional).
- **Exact NDK version** pinned during Phase 1a from `cargo tauri android init` output,
  then recorded back into `ANDROID.md` D22.
- **Baseline is already committed** — Phase 0 is now a re-verify, not a cleanup.
- **Already de-risked, don't undo:** `reqwest` is on `rustls-tls` (no
  `native-tls`/`openssl` anywhere in the lockfile), which avoids cross-compiling
  OpenSSL for Android entirely. A switch to `native-tls` would reintroduce a real
  Phase 1 blocker.
