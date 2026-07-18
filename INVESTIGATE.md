# Structural Audit — Goose Overlay (Kitty)

> Generated 2026-07-18. Exhaustive search of `src-tauri/src/` (Rust) and `src/` (TypeScript).

---

## 1. Files >500 LOC

| File | Lines | Concern |
|------|-------|---------|
| `src/stores/chatStore.ts` | **2313** | Contains chat state, streaming logic, artifact detection, error humanization, mode inference. Likely should split into substores (e.g., `chatStore.ts`, `streamStore.ts`, `artifactStore.ts`). |
| `src/themes/base.css` | **1612** | Single file for all layout/structure CSS. Acceptable given the CSS-custom-property theming model, but long. |
| `src-tauri/src/config/providers.rs` | **799** | Mixes data model (`ProviderProfile`), computation (`network_tier`), I/O (`keyring` CRUD), and connection testing. |
| `src-tauri/src/commands/session.rs` | **730** | 13 command functions in one file: session CRUD, prompt send/cancel, permission response, thinking effort, provider rebind. |
| `src-tauri/src/lifecycle/mod.rs` | **725** | Health loop, scheduled tasks, embedding model management, stop-all — four concerns in one file. |
| `src/components/settings/Recipes.tsx` | **714** | Entire recipe CRUD UI in one component — list, form, import/export. |
| `src/components/settings/Providers.tsx` | **643** | Provider list, form, key management, trust-tier dialog — all in one file. |
| `src/lib/types.ts` | **521** | 62 exported type aliases. Acceptable for a type hub. |

---

## 2. Modules with >10 Public Items

### Rust

| File | Public Items | Assessment |
|------|-------------|------------|
| `commands/session.rs` | **24** | 13 `pub async fn` + 11 type re-exports. Highest command surface area. |
| `commands/adaptive_pathway.rs` | **18** | 17 Tauri commands wrapping ~15 HTTP API calls. Surface is inherently large. |
| `adaptive_pathway/mod.rs` | **16** | 15 `pub async fn` HTTP client methods. |
| `lifecycle/mod.rs` | **16** | Too many responsibilities: process spawn, health loop, scheduled tasks, embedding model. |
| `config/providers.rs` | **14** | Profile model, CRUD, network_tier, test_connection. |
| `commands/mod.rs` | **13** | Re-exports only — fine by design. |
| `goosed/api.rs` | **13** | AcpClient struct + connect/request/ensure + 6 response types. |
| `windows.rs` | **12** | Window create/toggle/animate + deep-link helpers. |
| `config/mod.rs` | **11** | Config struct + load/save/merge. |

### TypeScript

| File | Exports | Assessment |
|------|---------|------------|
| `lib/types.ts` | **62** | Type hub — acceptable. |
| `lib/ipc.ts` | **40** | Wraps all `invoke()` calls — fine by design (architectural rule: only file calling `invoke`). |
| `stores/chatStore.ts` | **31** | See LOC concern above. |

---

## 3. Cyclic Dependencies — 5 cycles

### 🔴 CYCLE 1: `state` ↔ `lifecycle` (direct 2-node)

```
state.rs:15        → lifecycle::{ManagedProcess, StackStatus}
lifecycle/mod.rs:20 → state::AppState
```

**Files:**
- `src-tauri/src/state.rs:15` — `use crate::lifecycle::{ManagedProcess, StackStatus}`
- `src-tauri/src/lifecycle/mod.rs:20` — `use crate::state::AppState`

**Mechanism:** `state.rs` defines `AppState` whose fields reference types (`ManagedProcess`, `StackStatus`) defined in `lifecycle`. The `lifecycle` module then takes `&AppState` as parameter. This is a **true mutual dependency** — the type definitions live in `lifecycle` but the state struct that owns them lives in `state`.

### 🔴 CYCLE 2: `lifecycle` → `commands` → `lifecycle` (4-node)

```
lifecycle/mod.rs:504,506     → commands::new_session, commands::send_prompt
commands/setup.rs:10         → lifecycle
commands/window.rs:7         → lifecycle
commands/ollama.rs:8         → lifecycle
commands/adaptive_pathway.rs:11 → lifecycle
```

**Files:**
- `src-tauri/src/lifecycle/mod.rs` (inline calls to `crate::commands::new_session`, `crate::commands::send_prompt`)
- `src-tauri/src/commands/setup.rs:10` — `use crate::lifecycle`
- `src-tauri/src/commands/window.rs:7` — `use crate::lifecycle`
- `src-tauri/src/commands/ollama.rs:8` — `use crate::lifecycle`
- `src-tauri/src/commands/adaptive_pathway.rs:11` — `use crate::lifecycle`

**Mechanism:** The health/scheduled-tasks loop in `lifecycle` directly calls `commands::new_session` and `commands::send_prompt` to fire scheduled tasks. Every `commands/*.rs` that manages a child process imports `lifecycle` for `ManagedProcess`/`StackStatus`. The `lifecycle` → `commands` edge is the one that makes this a cycle — if that edge were broken (e.g., by extracting scheduled-task execution into a separate module that depends on `commands` but not `lifecycle`), the cycle collapses.

### 🔴 CYCLE 3: `lifecycle` → `config/providers` → `lifecycle/ollama_proc` → `lifecycle` (triangle)

```
lifecycle/mod.rs:108,126,639  → config::providers::{goosed_env, active_ollama_target, requires_local_ollama}
config/providers.rs:13        → lifecycle::ollama_proc
lifecycle/ollama_proc.rs:8    → lifecycle::ManagedProcess
```

**Files:**
- `src-tauri/src/lifecycle/mod.rs` (inline calls to `crate::config::providers::goosed_env`, `active_ollama_target`, `requires_local_ollama`)
- `src-tauri/src/config/providers.rs:13` — `use crate::lifecycle::ollama_proc`
- `src-tauri/src/lifecycle/ollama_proc.rs:8` — `use crate::lifecycle::ManagedProcess`

**Mechanism:** `lifecycle` reads provider config. `providers.rs` calls `ollama_proc::probe_version` for its `requires_local_ollama` check. `ollama_proc` imports `ManagedProcess` from parent `lifecycle`. The tightest link is `providers.rs` → `ollama_proc` — if `requires_local_ollama` were moved to `ollama_proc` itself, this triangle collapses.

### 🟡 CYCLE 4: `goosed/stream` → `notifications` → `windows` → `state` → `lifecycle` → `notifications` (chain)

```
goosed/stream.rs:13           → notifications
notifications.rs:8            → windows (is_overlay_hidden)
windows.rs:14                 → state::AppState
state.rs:15                   → lifecycle::ManagedProcess
lifecycle/mod.rs:613-614      → notifications (notify_if_hidden, Event::StackDegraded)
```

**Files:**
- `src-tauri/src/goosed/stream.rs:13` — `use crate::notifications`
- `src-tauri/src/notifications.rs:8` — `use crate::windows`
- `src-tauri/src/windows.rs:14` — `use crate::state::AppState`
- `src-tauri/src/state.rs:15` — `use crate::lifecycle::{ManagedProcess, StackStatus}`
- `src-tauri/src/lifecycle/mod.rs` (inline calls to `crate::notifications::notify_if_hidden`, `crate::notifications::Event::StackDegraded`)

**Mechanism:** Notifications need to know if the overlay is visible (`notifications → windows`). Checking window state requires `AppState` (`windows → state`). `AppState` carries `ManagedProcess` from `lifecycle`. The health loop in `lifecycle` also fires notifications. Each hop is shallow — no single file directly imports its own transitive dependency.

### 🟡 CYCLE 5: `goosed/stream` → `adaptive_pathway` → `state` → `goosed/api` → `goosed/stream` (chain)

```
goosed/stream.rs:271,274       → adaptive_pathway (base_url, record_outcome)
adaptive_pathway/mod.rs:11     → state::AppState
state.rs:13                    → goosed::api::AcpClient
goosed/api.rs:19               → goosed::stream
```

**Files:**
- `src-tauri/src/goosed/stream.rs` (inline calls to `crate::adaptive_pathway::base_url`, `record_outcome`)
- `src-tauri/src/adaptive_pathway/mod.rs:11` — `use crate::state::AppState`
- `src-tauri/src/state.rs:13` — `use crate::goosed::api::AcpClient`
- `src-tauri/src/goosed/api.rs:19` — `use crate::goosed::stream`

**Mechanism:** The streaming reply handler records outcomes via the adaptive-pathway API. The adaptive-pathway client needs `AppState` for config. `AppState` holds `AcpClient`. `api.rs` imports `stream` for its response types. The `api.rs:19` → `goosed::stream` import is the tightest link — moving the shared types to a third `goosed::types` module would break the cycle.

### Cycle summary

| Cycle | Type | Files | Actionable? |
|-------|------|-------|-------------|
| 1 `state` ↔ `lifecycle` | Direct 2-node | `state.rs`, `lifecycle/mod.rs` | **Yes** — move `ManagedProcess`/`StackStatus` out of `lifecycle` into a `types.rs` or into `state.rs` |
| 2 `lifecycle` → `commands` → `lifecycle` | Multi-node | 5 files | **Yes** — extract scheduled-task runner out of `lifecycle/mod.rs` |
| 3 `lifecycle` → `providers` → `ollama_proc` → `lifecycle` | Triangle | 3 files | **Yes** — move `requires_local_ollama` into `ollama_proc.rs` |
| 4 `goosed/stream` → `notifications` → ... → `lifecycle` → `notifications` | Long chain | 5 files | Worth noting, low urgency |
| 5 `goosed/stream` → `adaptive_pathway` → ... → `goosed/stream` | Long chain | 4 files | Worth noting, low urgency |

---

## 4. Feature Flag / `#[cfg]` Usage

### `cfg(desktop)` — injected by Tauri

| File | Line | Code | Verdict |
|------|------|------|---------|
| `lib.rs` | 52 | `#[cfg(desktop)]` wrapping single-instance plugin init | **OK** — Tauri v2 build.rs injects `desktop` cfg automatically. No `[features]` section needed. |

### Platform gates

| File | Line | Flag | Verdict |
|------|------|------|---------|
| `main.rs` | 2 | `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` | **OK** — Standard Windows GUI pattern. |
| `windows.rs` | 64 | `#[cfg(windows)]` on `overlay_target_position` actual impl | **OK** — Win32 FFI only compiles on Windows. |
| `windows.rs` | 92 | `#[cfg(not(windows))]` stub returning `None` | **OK** — Intentional compile-failsafe for cross-platform build. |
| `util.rs` | 10 | `#[cfg(windows)]` around `CommandExt` | **OK** — Windows-specific process flag. |
| `notifications.rs` | 63 | `#[cfg(windows)]` around `notify-rust` import | **OK** — Platform-specific notification crate. |

### `cfg(test)` — test modules

| File | Line | Verdict |
|------|------|---------|
| `config/mod.rs:357` | `#[cfg(test)]` mod | **OK** |
| `config/providers.rs:496` | `#[cfg(test)]` mod | **OK** |
| `config/recipes.rs:459` | `#[cfg(test)]` mod | **OK** |
| `config/recipe_yaml.rs:316` | `#[cfg(test)]` mod | **OK** |
| `config/scheduled_tasks.rs:47` | `#[cfg(test)]` mod | **OK** |
| `goose_config.rs:220` | `#[cfg(test)]` mod | **OK** |
| `goosed/stream.rs:296` | `#[cfg(test)]` mod | **OK** |
| `commands/file.rs:220` | `#[cfg(test)]` mod | **OK** |
| `lifecycle/mod.rs:697` | `#[cfg(test)]` mod | **OK** |
| `lifecycle/ollama_proc.rs:118` | `#[cfg(test)]` mod | **OK** |
| `log_capture.rs:16,104` | `#[cfg(test)]` use + mod | **OK** |
| `wizard.rs:386` | `#[cfg(test)]` mod | **OK** |

**No feature flag misuse found.**

---

## 5. Dead Code / Orphaned Branches

| File | Line | Pattern | Assessment |
|------|------|---------|------------|
| `windows.rs` | 92–95 | `#[cfg(not(windows))] fn overlay_target_position()` returning `None` | **Intentional** — compile-failsafe for hypothetical non-Windows build. Project is Windows-only (`bundle.targets = ["nsis"]`). |
| `lib.rs` | 52–60 | `#[cfg(desktop)]` single-instance plugin | **Not dead** — Tauri v2 always sets `cfg(desktop)` on desktop targets. |

---

## 6. Summary Counts

| Category | Count | Critical Items |
|----------|-------|----------------|
| **Files >500 LOC** | **8** | `chatStore.ts` (2313), `providers.rs` (799), `session.rs` (730), `lifecycle/mod.rs` (725) |
| **Modules with >10 public items** | **12** | `commands/session.rs` (24) most acute |
| **Cyclic dependencies** | **5** | Cycles 1–3 are actionable; 4–5 are long chains worth monitoring |
| **Feature flag misuse** | **0** | All `#[cfg]` usage is correct |
| **`#[cfg(test)]` modules** | **12** | All correctly placed |
| **Dead `#[cfg]` branches** | **1** | `windows.rs:92` — intentional failsafe |
| **No `[features]` section** | N/A | Not needed — Tauri injects `desktop`/`mobile` |

---

## Silent Errors Audit

### Confidence: 1 (informational) → 5 (critical)

### 1. `unwrap()` / `expect()` in async fn bodies — **Confidence: 5**

Every `Mutex::lock().unwrap()` in an async context panics on poison. With `panic = "abort"` in release profile (`Cargo.toml:70`), the **entire process aborts** — not just the task.

| File | Line | Context | Risk |
|------|------|---------|------|
| `lifecycle/mod.rs` | 90, 106, 125, 137, 348, 430, 487, 530, 634 | `start_stack`, `ensure_embedding_model`, health loop, `compute_status` | **5** — any poison → abort |
| `lifecycle/mod.rs` | 151, 170–171, 182, 259, 354, 382, 424, 573, 594, 635, 691–693 | Mutex updates in spawned tasks | **5** |
| `goosed/api.rs` | 283 | `ensure_client` — called by every session command | **5** — hot path |
| `commands/session.rs` | 317, 340, 435, 616, 669, 762 | `send_prompt`, `delete_session`, `rebind`, `clear_all` | **4** |
| `commands/ollama.rs` | 18 | `ollama_base()` — called by every ollama command | **4** |
| `commands/adaptive_pathway.rs` | 61, 73, 99–100, 114, 131, 148–149, 166–167 | Mutex accesses in async commands | **4** |
| `commands/provider.rs` | 41, 64, 103, 125, 145, 158, 163, 182 | Mutex in profile CRUD commands | **4** |
| `commands/recipes.rs` | 21, 101, 126, 160, 186, 248, 270 | Mutex in recipe commands | **3** |
| `notifications.rs` | 54 | `state.config.lock().unwrap()` in sync helper | **2** |
| `goosed/stream.rs` | 263 | Config lock in streaming path | **3** |

**Mitigation**: None. No `catch_unwind` guards, no poison recovery.

---

### 2. Unsafe blocks — **Confidence: 1**

One `unsafe` block at `windows.rs:72-79` — Win32 `SystemParametersInfoW` FFI call. Scoped, correct, single-threaded, no pointer escape. **No issue.**

---

### 3. Unbounded channel — **Confidence: 3**

`goosed/api.rs:71`: `mpsc::unbounded_channel::<Message>()` in the ACP writer pipeline.

```
request() → out_tx.send() → unbounded channel → out_rx → write.send() (WebSocket)
```

If the WebSocket stalls, messages queue with **no backpressure**. The sender is cloned into `AcpClient` (every session command) and the reader task — multiple sources enqueue concurrently.

**Mitigation**: Writer exits on `write.send().is_err()` → drops receiver → `out_tx.send()` fails back to senders. But memory is already consumed by then.

---

### 4. Missing `.await` on JoinHandle — **Confidence: 4**

All 7 `tokio::spawn` / `tauri::async_runtime::spawn` calls **discard the JoinHandle**. With `panic = "abort"` in release:

| File | Line | Task | Aborts on panic |
|------|------|------|-----------------|
| `goosed/api.rs` | 72 | ACP writer loop | Yes |
| `goosed/api.rs` | 94 | ACP reader loop | Yes |
| `lifecycle/mod.rs` | 81 | `start_stack` | Yes |
| `lifecycle/mod.rs` | 295 | Embedding model pull | Yes |
| `lifecycle/mod.rs` | 326 | Adaptive pathway health loop | Yes |
| `lifecycle/mod.rs` | 481 | Scheduler loop | Yes |
| `lifecycle/mod.rs` | 547 | Health loop (Mutex unwraps every 5s) | Yes |
| `commands/session.rs` | 342 | Prompt send retry wrapper | Yes |

In debug builds (`panic = "unwind"`), tokio catches the panic, logs it, task dies → acceptable. In release (`panic = "abort"`), **any panic kills the process**.

---

### 5. `Clone` on large types in hot paths — **Confidence: 2**

| File | Line | Type | Context | Risk |
|------|------|------|---------|------|
| `commands/config.rs` | 13 | `Config` (full) | `get_config` command — on-demand only | 1 |
| `goosed/stream.rs` | 67, 141, 375, 379 | `serde_json::Value` | Per-tool-call event forwarding | 2 — O(n) clone |
| `goosed/api.rs` | 280, 290 | `AcpClient` (Arc) | Cheap ref-count increment | 1 |

---

### 6. Additional: `std::thread::spawn` in `notifications.rs` — **Confidence: 2**

`notifications.rs:81`: Spawns blocking thread for toast response. `run_on_main_thread` error silently discarded at line 86 with `let _`. Acceptable for notification click handler.

---

### Silent Errors Priority Matrix

| # | Issue | Confidence | Fix Priority |
|---|-------|------------|-------------|
| 1 | 50× Mutex `unwrap()` in async + `panic=abort` → process death | **5** | **Immediate** — replace with `expect("poison")` or `?`-propagation |
| 2 | 8 fire-and-forget tasks, JoinHandle dropped, abort on panic | **4** | **High** — use `std::panic::set_hook` or drop `panic = "abort"` |
| 3 | Unbounded channel in ACP writer with no backpressure | **3** | Medium — switch to bounded (e.g. 1024) or add semaphore |
| 4 | `serde_json::Value` clones in stream handler per event | **2** | Low — optimization |
| 5 | Unsafe block | **1** | None |

---

### Top 5 structural issues to address

1. **`chatStore.ts` (2313 LOC)** — Extract stream handling, artifact detection, and error humanization into separate stores or hooks.
2. **Cycle 1: `state` ↔ `lifecycle`** — Pull `ManagedProcess`/`StackStatus` into a shared `types` module or into `state.rs` itself so `state` no longer depends on `lifecycle`.
3. **Cycle 2: `lifecycle` → `commands` → `lifecycle`** — Extract scheduled-task execution from `lifecycle/mod.rs` into a separate module that depends on `commands` but not the rest of `lifecycle`.
4. **Cycle 3: `lifecycle` → `config/providers` → `ollama_proc` → `lifecycle`** — Move `requires_local_ollama()` from `providers.rs` into `ollama_proc.rs` to break the triangle.
5. **`config/providers.rs` (799 LOC)** — Split into separate files: `providers/model.rs` (data types), `providers/keyring.rs` (secret storage), `providers/network.rs` (tier computation), `providers/connection.rs` (test_connection).

---

## Inefficiencies Audit

### Path classification: 🔥 hot (per-message / per-5s) | 🔸 warm (per-action) | ❄️ cold (once)

---

### 1. Allocations inside loops

#### 🔥 Hot path — stream handler per-message

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `goosed/stream.rs:83` | `sid.to_string()` per `session/update` notification | Each message chunk allocates a `String` for session id |
| `goosed/stream.rs:241-243` | `.to_string()` ×3 per tool call | Two tool name + one tool_call_id allocation per tool invocation |
| `goosed/stream.rs:384-393` | `.to_string()` ×2 + `.clone()` per permission request | Session id + tool call id allocation per every approval prompt |
| `goosed/stream.rs:29-35` | `cap_strings` — clone per key/value in tool-call JSON | O(n) clone in tool-output size; mitigated by 16KB cap |

#### 🔥 Hot path — health loop (every 5s forever)

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `lifecycle/mod.rs:363` | `format!("http://127.0.0.1:{port}")` in adaptive-pathway health loop | Allocates String every 5s for a port that never changes; cache-able |
| `lifecycle/mod.rs:637` | `cfg.ollama_base_url.clone()` in `compute_status` | String clone every 5s — also cache-able |

#### 🔥 Hot path — Ollama pull stream (per NDJSON line)

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `ollama/mod.rs:138-184` | `buf.push_str`, `.collect::<String>()`, `pull_id.clone()`, `model.clone()` | Per NDJSON line per model download. Worst-case: hundreds-thousands of small allocations per pull |
| `ollama/mod.rs:154` | `String::from_utf8_lossy(&chunk)` | Per-chunk allocation in stream |
| `ollama/mod.rs:164-165,175-176` | `pull_id.clone()`, `model.clone()` on every progress line | Unnecessary: these values are constant for the entire pull; clone once outside the `while` loop |

#### 🔸 Warm path — per-command / per-action

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `config/recipe_yaml.rs:77-108` | 4× `format!()` + `p.key.clone()` per parameter | Recipe validation — only on import/edit, not hot |
| `config/recipe_yaml.rs:136-139` | `.clone()` per template var in activities | Recipe validation, cold path |
| `config/recipe_yaml.rs:258-260` | `format!()` per unsupported schema key | Cold warning path |
| `commands/file.rs:154-161` | `format!()` per dedup collision | Only hit when filename conflicts (rare) |
| `commands/session.rs:328-335` | `push(json!(...))` without `with_capacity` | Per image attachment in prompt |
| `hotkey.rs:32-51` | `format!()` + `app.clone()` per accelerator | Few items, at registration only |
| `config/env_helper.rs:28-37` | `.to_string()` ×5, no `with_capacity` | On demand (Settings open) |

#### ❄️ Cold path

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `config/recipe_yaml.rs:227-233` | `format!()` per slug collision | Only on name collision (rare) |
| `commands/recipes.rs:213-219` | `format!()` per slug collision | Only on name collision (rare) |
| `lifecycle/goosed.rs:59-62` | `format!()` ×32 for secret generation | Once per goosed spawn |
| `lifecycle/mod.rs:495-509` | `app.clone()`, `task.cwd.clone()`, `task.prompt.clone()` | Per scheduled task fire |
| `log_capture.rs:88-93` | `.to_string()` ×3 per WARN/ERROR event | Low volume, acceptable |

**Mitigation note**: Adding `Vec::with_capacity` before loops that `push` is missing in `hotkey.rs:36`, `commands/session.rs:327`, `config/recipe_yaml.rs`, `commands/config.rs:61`. Low impact individually, but pervasive.

---

### 2. Blocking operations in async contexts

#### 🔥 Hot path — `std::sync::Mutex::lock()` on tokio worker threads

All `state.*.lock().unwrap()` calls in async fn bodies block the tokio worker thread. With 10 `std::sync::Mutex` fields in `AppState`, contention is structural:

| Group | Frequency | Fields locked |
|-------|-----------|---------------|
| Health loop (every 5s) | 5s interval | `config`, `goosed`, `stack_status` |
| Stream handler (per tool-call event) | Per-message | `config`, `goosed` (via `ensure_client`) |
| `send_prompt` command (every user message) | Per-message | `config`, `in_flight_sessions` |
| `ensure_client` (before every ACP call) | Per-command | `goosed` |

**All 50+ instances use `std::sync::Mutex`**, not `tokio::sync::Mutex`. The Mutex operations are short (no `.await` while held), so they won't deadlock — but a contended lock blocks the entire tokio worker thread, delaying all other tasks on that thread.

#### 🔥 Hot path — filesystem I/O in async fn

| File:Line | Op | Context | Risk |
|-----------|-----|---------|------|
| `commands/session.rs:168` | `std::fs::create_dir_all` | `new_session()` — every new session | Blocks tokio thread on FS write |
| `commands/session.rs:703` | `std::fs::remove_dir_all` | `delete_session()` | Blocks on recursive delete |
| `commands/session.rs:752` | `std::fs::remove_dir_all` | `clear_all_sessions()` | Blocks on bulk delete |
| `lifecycle/adaptive_pathway_proc.rs:99` | `std::fs::create_dir_all` | `ensure_running()` — startup + health loop | Blocks in process spawn path |

#### 🔸 Warm path — wizard/install I/O in async fn

| File:Line | Op | Context |
|-----------|-----|---------|
| `wizard.rs:198` | `std::fs::write` | `install_ollama()` |
| `wizard.rs:265` | `std::fs::create_dir_all` | `install_goose()` |
| `wizard.rs:343` | `Command::output()` | `install_adaptive_pathway()` |

These are correct-for-the-context (wizard runs once), but still block the tokio thread.

---

### 3. Unnecessary Arc/Mutex wrapping

#### 🔥 `state.rs` — `std::sync::Mutex` used for all fields, many read-heavy

| Field | Lock Type | Access Pattern | Suggested |
|-------|-----------|----------------|-----------|
| `config: Mutex<Config>` | Exclusive | **Read 20+ locations**, write only on settings save | `RwLock<Config>` |
| `goosed: Mutex<GoosedHandle>` | Exclusive | Read in every command + health loop, write on spawn | `RwLock<GoosedHandle>` |
| `ollama: Mutex<ManagedProcess>` | Exclusive | Read in health checks, write on start/stop | `RwLock<ManagedProcess>` |
| `active_session: Mutex<Option<Value>>` | Exclusive | Read-once / write-once per session switch | `RwLock` or `OnceLock` |
| `settings_target: Mutex<Option<Value>>` | Exclusive | Infrequent read/write | `RwLock` |
| `wizard_mode: Mutex<Option<String>>` | Exclusive | Infrequent | `RwLock` |
| `adaptive_pathway: Mutex<ManagedProcess>` | Exclusive | Read in health loop, write rarely | `RwLock` |
| `adaptive_pathway_status: Mutex<AdaptivePathwayStatus>` | Exclusive | Written every 5s, read on health check | `RwLock` or `AtomicU8` |
| `adaptive_pathway_embedding_status: Mutex<EmbeddingModelStatus>` | Exclusive | Written every 30s, read on health check | `RwLock` or `AtomicU8` |
| `stack_status: Mutex<StackStatus>` | Exclusive | Written every 5s, read on demand | `AtomicU8` (enum is `Copy`) |
| `in_flight_sessions: Mutex<HashSet<String>>` | Exclusive | Read + write on every message send/complete | Keep as `Mutex` (correct) |

**Impact**: With 10 exclusive Mutexes and 50+ lock sites, concurrent read contention is guaranteed in the async context. An `RwLock` on `config` alone would eliminate 20+ unnecessary exclusive lock acquisitions.

#### ✅ `goosed/api.rs` — Arc + tokio::sync::Mutex is correct

`Pending`, `Perm`, `Activity`, `ToolCalls` all use `Arc<Mutex<...>>` with `tokio::sync::Mutex`. These are held across `.await` points (correct) and shared between tokio tasks (Arc is necessary). **Not actionable.**

---

### 4. Repeated serialization / JSON construction

#### 🔥 Hot path — clone + re-serialize on retry

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `commands/session.rs:345,362` | `params.clone()` on prompt `Value` + re-serialized in retry | Wasted deep clone on the non-error path; double-serialization on retry path. Rare (only on "Internal error") but each prompt can be large. |

#### 🔸 Warm path — structural duplication

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `goosed/api.rs:142,187,245,251` | `json!({ "jsonrpc": "2.0", ... })` ×4 | Structural duplication of JSON-RPC envelope — code smell, no runtime cost |
| `goosed/stream.rs:93-167` | `json!({ "session_id": session_id, ... })` ×6 | Repeated shape in event emission — minor code dedup opportunity |
| `commands/session.rs` | `json!({ "sessionId": ... })` shapes ×14 | Heavy structural duplication across session command helpers |
| `ollama/mod.rs:109-199` | `PullProgress` constructor ×5 with `pull_id.clone()`, `model.clone()` | Clone fields once, not on every error path |

#### ❄️ Cold path — test-only serialization

| File:Line | Pattern | Impact |
|-----------|---------|--------|
| `config/mod.rs:394` | `serde_json::to_string(&c).unwrap()` | Test-only round-trip |
| `config/recipes.rs:491,502` | `serde_json::to_string` | Test-only round-trip |

---

### 5. Missing `Vec::with_capacity` — pervasive

| File:Line | Vec | Context | Items |
|-----------|-----|---------|-------|
| `config/recipe_yaml.rs:32` | `vars` | Template variable extraction | 0+ |
| `config/recipe_yaml.rs:78` | `v.errors` | Recipe validation errors | 0–many |
| `config/recipe_yaml.rs:103` | `v.warnings` | Recipe validation warnings | 0–many |
| `config/recipe_yaml.rs:260` | `warnings` | Unsupported schema key warnings | 0–few |
| `hotkey.rs:36` | `errors` | Hotkey registration errors | 0–few |
| `commands/session.rs:327` | `prompt` | Message images | 0–few |
| `commands/config.rs:61` | `user` | Theme file listing | 0–few |

**Impact**: Negligible individually. In cold paths (config load, recipe validation), the reallocation cost is invisible. In the prompt-building path (`commands/session.rs:327`), images are 0–5 items typically. Not actionable.

---

### 6. Efficiency Priority Matrix

| # | Issue | Path | Impact | Fix Priority |
|---|-------|------|--------|-------------|
| 1 | `std::sync::Mutex` in async blocks tokio workers (50+ sites) | 🔥 | **High** — every async command, health loop, stream handler | Migrate `AppState` fields to `RwLock` where reads dominate; migrate `config`, `goosed`, `ollama`, `adaptive_pathway` first |
| 2 | `params.clone()` + re-serialize on retry in `send_prompt` | 🔸 Warm | Medium — wasted deep clone on common path | Use `Arc<Value>` or restructure to avoid clone until retry branch |
| 3 | String allocations per NDJSON line in pull stream (`ollama/mod.rs`) | 🔥 | Medium — per-layer allocations during model pull | Cache `pull_id`/`model` outside the `while` loop; use `with_capacity` on `buf` |
| 4 | `format!()` every 5s in health loops (`lifecycle/mod.rs:363`) | 🔥 | Low — tiny allocation, but forever | Cache `format!("http://127.0.0.1:{port}")` in a local var or lazy_static |
| 5 | `sid.to_string()` per `session/update` in stream handler | 🔥 | Low — allocation per message chunk (expected cost) | Acceptable; string is small |
| 6 | `.to_string()` ×3 per tool call in stream | 🔥 | Low | Acceptable; part of event emission cost |
| 7 | Missing `Vec::with_capacity` in 7 locations | ❄️/🔸 | Negligible | Low-value, cold paths |
| 8 | Structural `json!()` duplication | 🔸 | Negligible | Code hygiene, no runtime cost |
| 9 | Goosed secret generation `format!()` ×32 | ❄️ | Negligible | Once per process lifetime |

---

## Responsiveness Audit

### SEVERE — user notices every time

| Pattern | File:Line | Issue | Perception Impact | Fix Complexity |
|---------|-----------|-------|-------------------|----------------|
| No loading state on async button | `components/chat/MessageItem.tsx:88-101` | Branch/Regenerate/Export buttons have no `disabled` state. Duplicate clicks send duplicate IPC calls | User thinks click didn't register, re-clicks → forks multiple sessions, queues duplicate prompts | Easy — add `busy` state + `disabled` prop |
| No loading state on approval buttons | `components/chat/ApprovalPrompt.tsx:47-61` | Approve/Deny/Allow-always buttons have no `disabled` state | Double-click sends duplicate permission responses to goosed; user can't tell if click landed | Easy — add local `submitting` state |
| Forced layout on every keystroke | `components/chat/Composer.tsx:146-154` | `onChange` reads `scrollHeight` (forces layout) + sets `style.height` on every keystroke | Typing latency on slower machines; each keystroke triggers layout + repaint before React render | Easy — use `requestAnimationFrame` or CSS `field-sizing: content` |
| Unbounded channel with no backpressure | `goosed/api.rs:71` | `mpsc::unbounded_channel::<Message>()` for ACP WebSocket outbound | If remote provider stalls, memory grows without bound; app becomes sluggish or OOM | Medium — switch to bounded `channel(1024)` + `send().await` |
| Silent session cleanup swallows failure | `stores/chatStore.ts:2005-2006,2014-2015` | `.catch(() => {})` on `ipc.deleteSession()` — if goosed rejects the delete, orphan sessions accumulate | Abandoned sessions leak in goosed; session list shows stale entries user can't interact with | Medium — log error, surface in stackStore |
| `cap_strings` clones entire tool JSON tree | `goosed/stream.rs:20-36` | Recursive clone of every key/value in tool-call JSON on every `tool_call_update` event | Deeply nested tool output (e.g. JSON from a file-read tool) causes brief async-task pause per update | Medium — clone-avoiding traversal |
| No loading state on session resume | `components/sessions/RecentSessions.tsx:59-64`, `SessionList.tsx:391-393` | Click to resume session has no loading feedback; popover closes immediately | User sees nothing happen for hundreds of ms; re-clicks, loads the session twice | Easy — add `loading` state + disable row during load |

### MODERATE — noticed under load

| Pattern | File:Line | Issue | Perception Impact | Fix Complexity |
|---------|-----------|-------|-------------------|----------------|
| `reqwest::Client` built per API call (22 sites) | `adaptive_pathway/mod.rs:14-17` + 21 other locations | Each `Client::builder().build()` allocates TLS/proxy/connector state (~1-5ms each) | Under burst (session replay, rapid commands), repeated client construction wastes cycles | Medium — store in `AppState` or `OnceCell` |
| `std::fs::remove_dir_all` blocks tokio thread | `commands/session.rs:703,752` | `delete_session` / `clear_all_sessions` do recursive dir delete without `spawn_blocking` | UI freezes during session delete if session has many files; frame drop on each delete | Easy — wrap in `tokio::task::spawn_blocking` |
| `keyring::get_password()` blocks on OS IPC | `config/providers.rs:178,183,311` | Keyring credential read does Windows Credential Manager IPC inside async fn | Provider activation / connection test hangs if Credential Manager is slow; blocks tokio thread | Medium — cache secrets in `AppState` or use `spawn_blocking` |
| `pointermove` on SessionList not throttled | `components/sessions/SessionList.tsx:121` | Drag-to-reorder sends 60+ events/sec, each triggering `setDragOverFolder` → re-render | Janky drag experience with large session lists; rubber-banding feel | Easy — throttle to 30fps with rAF |
| Inline arrows in `memo(MessageItem)` defeat memoization | `MessageItem.tsx:55,88,93,99` | `onClick` closures recreate per render; `React.memo` only protects unchanged items | Each message item that changes (e.g. streaming) causes 7 new closure allocations; minor GC pressure | Easy — wrap handlers in `useCallback` |
| `ThinkingBox` reasoning defaults closed (spec mismatch) | `components/chat/ThinkingBox.tsx:10,24` | "never auto-expands, even while streaming" — Phase 10 requires auto-expand during streaming reasoning | User cannot see model thinking as it happens; feels like longer wait before any output | Easy — auto-open during streaming, close on completion |
| `useProgressStage` creates/destroys timer per delta | `components/chat/useProgressStage.ts:32-34,41` | `setTimeout` + `clearTimeout` on every reasoning delta (1500x for heavy reasoning) | Timer churn adds GC pressure during heavy reasoning bursts; minor jank on low-end hardware | Easy — single timer with interval check |
| No debounce on session search input | `components/sessions/SessionList.tsx` | `setQuery` fires on every keystroke with no debounce | Rapid typing triggers IPC call (`listSessions`) on every key; backlog of stale results | Easy — add 150ms debounce |
| `JSON.stringify` on large tool output in render path | `components/chat/ToolCallCard.tsx:7-12,31,37` | `JSON.stringify(v, null, 2)` on tool params/result blocks main thread | 500KB shell output takes noticeable ms to stringify; frame drop on each tool call render | Medium — truncate or defer with `useDeferredValue` |
| `reqwest::Client::builder()` built twice in same spawned task | `lifecycle/mod.rs:282-306` | Two `expect("reqwest client")` calls in `ensure_embedding_model`'s spawned pull task | Wasted TLS initialization; seconds of unnecessary startup time per pull | Easy — hoist to one `Client` |

### Top 3 ROI Recommendations

**Rank 1 — Add `disabled` states to async buttons** (SEVERE, easy fix)
- Fix `MessageItem.tsx:88-101`, `ApprovalPrompt.tsx:47-61`, `RecentSessions.tsx:59-64`
- **Why**: Most visible responsiveness failure — user clicks, nothing happens, re-clicks. Fixing eliminates "is it working?" confusion and prevents duplicate operations.
- **Files**: 3 TSX files, ~20 lines changed
- **ROI**: Highest — directly addresses "feels unresponsive" report

**Rank 2 — Wrap `reqwest::Client` creation in a `OnceCell` / reuse pattern** (MODERATE, medium fix)
- Fix `adaptive_pathway/mod.rs:14-17` and consolidate 22 `Client::builder()` sites
- **Why**: Each client build is 1-5ms of TLS setup. Under burst (session replay, rapid commands), 22× this waste adds up. A shared client also enables connection pooling (keep-alive).
- **Files**: 1 shared helper + update all callers
- **ROI**: High — eliminates repeated TLS setup across the entire backend

**Rank 3 — Wrap blocking `std::fs` ops in `spawn_blocking`** (MODERATE, easy fix)
- Fix `commands/session.rs:168,703,752`, `lifecycle/adaptive_pathway_proc.rs:99`
- **Why**: Filesystem I/O on the tokio thread blocks all other tasks. `create_dir_all`/`remove_dir_all` on session operations are user-triggered, so the freeze is directly perceptible.
- **Files**: 2 Rust files, ~6 lines changed
- **ROI**: High — eliminates UI-freeze-on-delete-session
