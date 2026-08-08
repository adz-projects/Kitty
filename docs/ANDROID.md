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

---

## 1. Decisions (definitive register)

| # | Decision | Where it lands |
|---|----------|----------------|
| D1 | **In-process llama.cpp, day 1, both OSes.** `bigtiny_rust` links `llama_cpp`. No Ollama anywhere: chat, compaction, and desktop embeddings all go through the daemon. | §2, §4 |
| D2 | **Model is pinned per chat.** Locked in at chat creation; changes only on **new chats**. No mid-stream swap, ever. | §4.1 |
| D3 | **Scheduled tasks default to the summarizer model**; per-task override in Settings. | §7 |
| D4 | **Embeddings on desktop only.** AP calls new `POST /api/embeddings`; `AP_EMBED_OLLAMA_URL` re-points to the daemon port. On Android, AP's pipeline runs with its **built-in hash-space embeddings** (`HASH_EMBED_MODEL` fallback) — no embed model, no `/api/embeddings` call. | §3.1 |
| D5 | **In-app model downloads** from **HuggingFace** and the **Ollama registry** (manifest → blob → reassembled GGUF). Range+resume+sha256. | §5 |
| D6 | **Exposed llama.cpp engine knobs** + **Quick Presets** (Precise/Balanced/Creative). | §6 |
| D7 | **No proactive disk quota.** Only a low-free-space warning; manual delete. **Hard refusal when `free_space < model_size × 1.5`**. | §5 |
| D8 | Adaptive-Pathway runs **on both OSes, in-process inside the BigTiny daemon** (linked crate, MCP `in_process` "pathway" server — never a separate process). Windows: daemon is a Kitty-managed sidecar; Android: daemon is in-process. Android runs the *full* AP engine (recall/surface/consolidate) with hash-space embeddings (D4); MCP tool servers: **stdio sidecar on Windows**, **`in_process` on Android**. | §2 |
| D9 | Frontend = 2 windows (`overlay` + `hub`); settings/wizard fold in-page. Android = same hub, mobile-rendered. | §8 |
| D10 | **Model card** (size/RAM/VRAM/backend/one-tap new-chat default) + **"Recommended for this device" badge** computed from the fit function. | §6.3 |
| D11 | Load-time param changes schedule an **automatic engine restart**: after the current LLM generation completes, or immediately if nothing is generating. Sessions show a non-blocking "restarting" chip. | §6.4 |
| D12 | **Summarizer fallback = the session's chosen model** (may be a cloud provider). Chain: `Local → session model → explicit error`. **No Gemini Nano.** | §4.3 |
| D13 | **Model health always visible** in the model picker (green/red dot). | §6.3 |
| D14 | **Wizard is platform-agnostic**; only Android difference = permissions step. | §8.3 |
| D15 | **Shared design tokens** consumed by both render paths; **dark/light follows the OS** (manual override allowed). | §8.4 |
| D16 | **Safe-area tokens** (`--safe-*`): real `env()` on Android, `0` on desktop. | §8.4 |
| D17 | **Font parity**: Android accessibility font scale and Windows zoom both drive one shared `--font-scale`/rem ramp; **full px→rem sweep of `base.css`**. | §8.4 |
| D18 | **Android = cloud-chat only.** Local engine on Android runs **solely the summarizer**. No local chat, no local model picker, no per-chat local choice. One resident local model. | §2.2, §4.2 |
| D19 | **`flash_attn` auto-detected** at engine init; never a user toggle (read-only card diagnostic `"off"`/"on (<backend>)"). | §3.3 |
| D20 | **`auto`/`-1` select backend**: `select_backend()` returns `Cuda|Vulkan|Cpu`; fit/badge/VRAM math uses the *selected* backend's VRAM bank. | §3.3 |
| D21 | **Windows multi-window is preserved**: two (or more) hub windows may be open at once, each with its **own independent session and its own pinned model**. Android stays single-window. | §8.1, §4.2 |

---

## 2. Architecture

### 2.1 Runtime split

| Platform | Chat | Summarizer | Embeddings | MCP tool servers | AP |
|----------|------|-----------|------------|------------------|----|
| Windows | Local (llama) **and** cloud providers | Local (llama) → session-model fallback | Local (via daemon endpoint) | stdio sidecars | ✓ (in-process in daemon, semantic embeddings) |
| Android | **Cloud providers only** | Local (llama) → session-model fallback | — | `in_process` builtins | ✓ (in-process in daemon, hash-space embeddings) |

### 2.2 Local model policy per platform (D18)
- **Android**: exactly **one resident local model** — the summarizer (default
  `LFM2.5-1.2B`). Chat never loads a local model, so there are no per-chat swaps
  (keeps the 1-slot RAM floor and warm-cache benefits). The default GGUF is
  **downloaded on first use** (not bundled in the APK); until it exists, the
  summarizer falls back to the session model per D12.
- **Windows:** 2 resident slots — `chat` and `summarizer/embeddings`. Chat slot
  only loads when a chat pins a local model (D2); otherwise chat talks to cloud
  providers and the slots stay only on the summarizer model.

### 2.3 Adaptive-Pathway process model (D8)

AP is never a standalone process on either OS — it is the **linked-in
`adaptive_pathway` crate** inside the BigTiny daemon, served as the `in_process`
"pathway" MCP row (`ensure_builtin_servers` in src-tauri) and direct
`PathwayEngine` calls.

- **Windows:** the daemon itself is a Kitty-managed sidecar
  (`lifecycle/bigtiny_proc.rs`); AP lives inside it. Semantic embeddings come
  from `POST /api/embeddings` (daemon engine).
- **Android:** the daemon is in-process (Android can't `exec()` a sidecar), so
  AP is naturally in-process too — **no code change separates the Android build
  from Windows** on the AP side; the only functional difference is hash-space
  embeddings (D4) since no embed model is loaded. Feature parity: recall,
  surfaced-assumption ordering, consolidation, PIP (confirm/sub/uncertain),
  suppression — identical behavior, just the vector space backing them differs.

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
| Windows | chat pool + 1 summarizer | chat pool: **one slot per active local-chat window** (D21); summarizer/embeddings shared | idle slot only |
| Android | 1 | summarizer | (no competitor) |

- A session pins its model at creation (D2). A new session requesting a missing
  model **adds a chat-slot on demand** (Windows) or **queues behind any busy slot**
  (Android's summarizer-only case); frontend shows **"Model loading…"** in the
  new-chat composer until ready. In-flight streams are **never aborted**.
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

## 9. Default model

- `LiquidAI/LFM2.5-1.2B-Instruct-GGUF` → file `lfm2.5-1.2b-instruct-q4_k_m.gguf`
  (~731 MB, arch `lfm2`, native 32k ctx, ~950 MB min memory).
  - Baseline sampling = the Precise preset.
- **Fallback model = Qwen3-1.2B q4_K_M** (used only if `lfm2` fails to load in
  the pinned `llama_cpp` version; see Phase 1).
- GGUF cache lives in app-local dir (Android backup-excluded).

---

## 10. Phases & acceptance criteria (Do this order)

### Phase 1 — Cross-compile spike + Android toolchain bounds
- **Goal:** prove `llama_cpp` + `wasmtime` + `sqlx` cross-compile for
  `aarch64-linux-android`; pin `llama_cpp`; confirm arch `lfm2` loads; verify
  `-1` behaves as "all layers" across packaged backends.
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
  with llama + wasmtime + sqlx **+ `adaptive-pathway` (path dep, `in_process`
  MCP row — no separate AP artifact)**; a `lfm2.5…q4_k_m.gguf` load via
  `llama_cpp` returns
  tokens; **app boots on an API 26–34 emulator AND one physical arm64 device with
  zero `UnsatisfiedLinkError`/`dlopen` failures**; `readelf -d`/`nm -u` on the
  produced `.so` confirms no unresolved external symbols. Fallback decision
  tombstones in `docs/ANDROID.md` if `lfm2` fails.

### Phase 2 — Daemon engine (Windows first)
- **Acceptance:** `cargo test` + `cargo clippy` clean; chat through `LocalEngine`;
  compaction through `LocalEngine`; `/api/embeddings` round-trip via AP (Windows);
  AP engine (recall/consolidate/surface) testable in-process **without**
  `/api/embeddings` (hash-space path, which Android will use); secrets
  through the keyring path; Ollama removed from the whole tree (grep `ollama`).
- Files: §3 + deletion of `src-tauri/ollama/`, `commands/ollama.rs`,
  `lifecycle/ollama_proc.rs`, `lifecycle/summarizer_model.rs`,
  `config/env_helper.rs`, `state.ollama`, `providers::active_ollama_target`.

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
  verified on-device (hash-space) and on desktop**.

### Phase 8 — Packaging
- `plugins/build.py` must NOT freeze Rust sidecars for Android. `externalBin`
  removed for Android; daemon + kitty-* linked in (AP comes along inside the
  daemon crate — no separate artifact). Signed AAB + Windows installer.
- `AGENTS.md` commands gain an `android` lane; scrub OLLAMA references in `docs/`.

---

## 11. Risks / notes

- `llama_cpp`+`wasmtime`+`sqlx` on aarch64 (Phase 1 gate). `llama-cpp-2rs`
  (crates.io fork) stays a *decision*, not a blocker — the `LocalEngine` boundary
  isolates a backend swap.
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
- **AP on Android uses hash-space embeddings (D8/D4):** semantic recall quality
  is lower than Windows until a local embed model is added — acceptable for the
  cloud-chat-only Android path; the pipeline (recall/surface/consolidate) is
  identical and shares code, so a future embedder slot lifts Android AP without
  a rewrite.
- **Windows multi-window RAM:** two concurrent chat windows pin two potentially
  different local models → two chat slots + summarizer resident. Weights are
  shared when models match; the chat-pool evicts the **idle** slot first if RAM is
  tight, and the fit/badge math (§3.3) underpins a low-RAM warning.