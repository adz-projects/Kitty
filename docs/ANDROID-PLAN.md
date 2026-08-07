# Kitty Android & Local-Inference — Execution Plan

Status: **Execution plan. Approved.** Grounded in `docs/ANDROID.md` (the spec) against
the current repo state. Do the phases in order — Phases 1a/1 are the hard upstream gate.

## Current-repo reality check (verified)

- `plugins/bigtiny_rust/` exists, builds a `bigtiny-daemon` bin with chat/compaction/
  provider/MCP already — but **Ollama is still wired everywhere** and `llama_cpp` is
  **not yet a dependency**.
- Daemon currently spawns the frozen Python `bigtiny-daemon` (`src-tauri/src/lifecycle/bigtiny_proc.rs`);
  the Rust daemon is the replacement target.
- Windows windows: `main/overlay/settings/wizard/screenshot-select` — **no `hub`** yet;
  `vite.config.ts` is multi-page.
- No Android config / NDK in `tauri.conf.json`; `externalBin` lists only Windows sidecars.
- **Uncommitted in-flight work exists** (modified `tauri.conf.json`, `Cargo.lock`s,
  binaries, `package.json`, untracked `docs/ANDROID.md`). Commit/stash before touching
  anything so you can iterate and roll back.

## Toolchain / environment facts (verified)

- No JDK, no Android SDK/NDK, no `cargo-ndk`, no `aarch64-linux-android` rust target,
  no `adb`, no `cmake` on PATH. Only `x86_64-pc-windows-msvc` rust target.
- Host is x86_64 Windows → **cannot run an arm64 AVD**; physical arm64 device is the
  ARM-accurate gate coverage (user confirmed a device is available).
- 94.5 GB free disk. `winget` available (no choco). Rust 1.96.1, cargo 1.96.1.
- **Decisions locked:** CLI-only toolchain (no Android Studio GUI, no AVD image by
  default); JDK 17 pinned; physical arm64 device = ARM gate.

---

## Phase 0 — Stabilize baseline

Commit/stash the in-flight uncommitted changes. Verify green before touching anything.

- `git status` → commit or stash.
- `pnpm test` and `pnpm lint`
- `cargo test` + `cargo clippy` in `src-tauri/` and `plugins/bigtiny_rust/`

**Exit:** a clean, reproducible green baseline.

---

## Phase 1a — Android dev environment bootstrap (CLI)

Ordered steps (winget + rustup + sdkmanager):

1. **JDK 17** — `winget install Microsoft.OpenJDK.17`; set `JAVA_HOME`.
2. **Android cmdline-tools** — `winget install Google.AndroidSDK` (or fetch
   `commandlinetools-win-*.zip`); set `ANDROID_HOME` + `ANDROID_SDK_ROOT`; run
   `sdkmanager --licenses`.
3. **SDK components** — `sdkmanager --install "platform-tools" "platforms;android-34"
   "build-tools;34.0.0" "ndk;27.*"`. Pin the NDK version `cargo tauri android init`
   actually wants.
4. **CMake** — `winget install Kitware.CMake` (native `llama_cpp`/ggml NDK CMake builds).
5. **Rust android target + cargo-ndk** — `rustup target add aarch64-linux-android`;
   `cargo install cargo-ndk`. Set `ANDROID_NDK_HOME`.
6. **adb + device wiring** — `platform-tools` brings `adb`; enable **USB debugging** on
   the device; `adb devices` lists it.
7. **Toolchain self-test** — `cargo ndk -t arm64-v8a build` on a trivial crate exits 0;
   `pnpm tauri android init` succeeds; `cargo tauri android build --target aarch64`
   reaches APK packaging.

**Acceptance (1a):** `adb get-state` == `device`; cargo-ndk builds to arm64; Tauri
Android build completes. Budget ~10–15 GB disk.

---

## Phase 1 — Cross-compile spike (gate; highest risk)

- Add `wasmtime` + `llama_cpp` (feature-gated) + Android build profile to `bigtiny_rust`.
- `cargo build --target aarch64-linux-android` for llama + wasmtime + sqlx.
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
- **Acceptance:** `cargo test` + `cargo clippy` clean; chat + compaction run through
  `LocalEngine`; embeddings round-trip via AP; secrets via keyring; no live `ollama`
  references.

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
- `SecretStore` — Keystore backend for provider keys / HF token.
- GGUF first-use download path; no local chat picker; backward-compat.
- Download-while-backgrounded: dataSync FGS + visible notification, `ConnectivityManager`
  `NetworkCallback` for Wi-Fi↔Cellular handoff, byte-offset resume (§5.2). Multi-GB GGUF
  pull survives Deep Doze and network changes without corruption.
- **Acceptance:** build + install on device; summarizer cloud fallback fires; wizard
  grants perms; OEM battery restart resumes; **a model download interrupted by airplane
  mode resumes from the same byte after reconnect**.

---

## Phase 8 — Packaging & docs

- `plugins/build.py` must **not** freeze Rust sidecars for Android; `externalBin` removed/
  gated for Android; daemon + `kitty-*` linked in. Signed AAB + Windows installer.
- `AGENTS.md` gains an `android` lane; scrub OLLAMA references in `docs/`.

**Acceptance:** Android AAB builds/signs with daemon linked in; Windows installer still
builds; commands table updated.

---

## Per-gate commands

```
git commit                                                    # Phase 0 land
cargo test && cargo clippy                                    # src-tauri + bigtiny_rust
cargo ndk -t arm64-v8a build --manifest-path plugins/bigtiny_rust/Cargo.toml
adb devices                                                    # 1a wiring check
cargo build --target aarch64-linux-android --manifest-path plugins/bigtiny_rust/Cargo.toml   # Phase 1 gate
pnpm test && pnpm lint                                         # after each frontend phase
```

## Key risks / deferred items

- **Phase 1 (llama_cpp + wasmtime + sqlx on aarch64, NDK static linkage) is the
  make-or-break gate** — validate a working cdylib before any UI work.
- **wasmtime** is not currently a dependency; must be added (feature-gated) for the spike.
- **CUDA toolkit deferred** to Phase 2 desktop GPU testing (~3 GB, not needed for compile
  gate). Vulkan runtime on Windows host.
- **Emulator:** device-only satisfies the arm64 gate; add an x86_64 host AVD only if a
  no-hardware path is wanted (optional).
- **Exact NDK version** pinned during Phase 1a from `cargo tauri android init` output.
- **Uncommitted baseline** must be committed/stashed in Phase 0 before any edits.
