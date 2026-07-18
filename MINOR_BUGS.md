# Minor-but-Valid Bugs & Polish

> Curated from the Deepseek V4 Flash audit (`INVESTIGATE.md`), **re-verified against
> source on 2026-07-18**. Only items confirmed real are listed here. Each entry notes
> the verified location, why it's genuine, and a suggested fix.
>
> **Update 2026-07-18 (second pass):** items 1–5, 6 (partial), 8, and 9 below are now
> **fixed** — kept in place (marked ✅) as a record of what was done and why, rather
> than deleted, since a couple of the "fixes" turned out to be narrower than the
> audit's framing (see each entry's **Fixed:** note). `cargo fmt`/`clippy`/`cargo test`
> (78 passed) and `tsc --noEmit`/`eslint` all clean after these changes.
>
> Severity legend: 🟠 worth doing soon · 🟡 nice-to-have · ⚪ cosmetic/hygiene
>
> **Not included** (correctness bugs already fixed in an earlier pass): ThinkingBox
> Phase-10 auto-expand, ApprovalPrompt double-submit, MessageItem duplicate
> Branch/Regenerate/Export, Composer forced-layout-per-keystroke (former #7).
>
> **Not included** (audit findings that did NOT survive verification): see
> "Rejected on verification" at the bottom.

---

## Backend (Rust)

### 1. ✅ 🟡 `reqwest::Client` constructed per-call instead of shared
- **Where:** 22 construction sites across 10 files — `adaptive_pathway/mod.rs`, `commands/adaptive_pathway.rs`, `commands/setup.rs`, `config/providers.rs`, `lifecycle/adaptive_pathway_proc.rs`, `lifecycle/mod.rs`, `lifecycle/ollama_proc.rs`, `ollama/mod.rs`, `openrouter/mod.rs`, `wizard.rs` (verified: 22 `Client::builder`/`Client::new` sites).
- **Why real:** each `Client::builder().build()` allocates fresh TLS/connector state (~1–5ms) and forfeits connection-pool keep-alive. Under bursts (session replay, rapid adaptive-pathway commands) this repeats needlessly.
- **Fixed:** added `util::http_client()` — one process-wide `reqwest::Client` behind a `OnceLock`, built once with `user_agent("kitty-app")`. All 22 sites now clone it instead of building their own. Per-call custom timeouts (2s/3s/5s/10s, which varied by call site) are preserved via `.timeout(Duration)` on the request builder rather than the client, so behavior is otherwise unchanged. `ollama_proc::probe_version`/`has_any_model`/`has_model_tag` and `adaptive_pathway_proc::probe_health` — the four probe helpers reused across many call sites — now hardcode a shared `PROBE_TIMEOUT`/inline timeout instead of taking it from the caller's client, since the old per-call values (2s/5s) were incidental, not deliberate distinct SLAs.

### 2. ✅ 🟡 Two `reqwest::Client`s built in one spawned task
- **Where:** `lifecycle/mod.rs` `ensure_embedding_model` (client built before `spawn`, and again inside the spawned task after the pull completes).
- **Why real:** redundant TLS init inside a single logical task; subset of #1.
- **Fixed:** folded into #1 — both call `util::http_client()` now.

### 3. ✅ 🟡 Blocking `std::fs` in async fn bodies (no `spawn_blocking`)
- **Where:**
  - `commands/session.rs` — `create_dir_all` in `resolve_cwd`/`new_session`; `remove_dir_all` in `delete_session` and `clear_all_sessions`.
  - `lifecycle/adaptive_pathway_proc.rs` — `create_dir_all` in `ensure_running`.
- **Why real:** recursive delete/create on the tokio worker thread stalls every other task on that thread; `delete_session`/`clear_all` are user-triggered, so the stall is perceptible on large session dirs.
- **Fixed:** every call now runs inside `tokio::task::spawn_blocking`. `resolve_cwd` became `async fn` (its one caller, `new_session`, was already async).

### 4. ✅ 🟡 `keyring::get_password()` (Windows Credential Manager IPC) on the async thread
- **Where:** `config/providers.rs` (secret reads in `test_connection`'s openrouter/anthropic/openai branches) and `commands/provider.rs::openrouter_credits`.
- **Why real:** Credential Manager is synchronous OS IPC; a slow CM blocks the tokio worker during connection tests / credit checks.
- **Fixed:** added `providers::get_secret_async` (wraps `get_secret` in `spawn_blocking`) and switched the four async-context call sites above to it.
- **Scoped narrower than the audit's framing:** `goosed_env()` (called during provider *activation*/goosed restart, the other surface the audit named) still calls the sync `get_secret` directly, under a held `std::sync::Mutex<Config>` guard. Left as-is: `goosed_env` is a plain sync fn exercised by 15 existing unit tests expecting a synchronous `&Config -> Vec<(String,String)>` signature, and it's called once per goosed (re)spawn — not a hot or frequently-repeated path. Restructuring it to drop the lock, hop to `spawn_blocking`, and reacquire would be a larger, riskier refactor for a single rare call; not worth it for a 🟡 item.

### 5. ✅ 🟡 Unbounded ACP writer channel (no backpressure)
- **Where:** `goosed/api.rs` — `mpsc::unbounded_channel::<Message>()`.
- **Why real:** if the goosed WebSocket stalls, outbound messages queue without bound.
- **Fixed:** switched to `mpsc::channel(ACP_OUT_CHANNEL_CAPACITY = 64)`. `AcpClient::respond`/`notify` are now `async fn` (`.send().await`), and `stream.rs`'s `out: &mpsc::Sender<Message>` parameters/call sites updated to match. Their two callers (`commands/session.rs`'s `cancel_prompt`/`respond_permission`, both already `async` commands) now `.await` them.

### 5a. ⚪ `cap_strings` deep-clones the whole tool-update JSON tree per event
- **Where:** `goosed/stream.rs:141` calls `cap_strings(update, …)` on every `tool_call`/`tool_call_update`; `cap_strings` (`:20-36`) rebuilds the entire `Value` tree even when nothing exceeds the 16KB cap (verified).
- **Why real (minor):** one extra O(tree) allocation pass per tool event, on a path that already serializes O(n) to emit — so it roughly doubles the per-event copy cost for large tool outputs. Correctness is fine.
- **Audit rated this SEVERE — it is not.** Output is capped, the clone is bounded, and it's a warm-ish (per-tool-event) path, not per-token.
- **Fix (optional):** have `cap_strings` return `Cow<Value>` / only allocate sub-trees that actually contain an over-cap string. Non-trivial for little gain; leave unless profiling flags it.

### 5b. ⚪ `send_prompt` clones `params` on every send for a rare retry
- **Where:** `commands/session.rs:345` — `params.clone()` passed to `request_session_prompt`, retained only for the "Internal error" retry at `:362` (verified).
- **Why real (minor):** deep-copies the prompt (incl. base64 image data) on the common no-retry path. But it's **one clone per user message** (warm path), not a hot loop — imperceptible in practice, and the copy is unavoidable if the value must survive for a possible retry.
- **Fix:** none recommended. Documented so it isn't re-flagged.

### 6. ⚪ Per-iteration allocations in hot loops
- **Where:**
  - `ollama/mod.rs` pull loop — `pull_id.clone()` / `model.clone()` on every NDJSON progress line.
  - `lifecycle/mod.rs`'s two health loops — `format!("http://127.0.0.1:{port}")` and `ollama_base_url.clone()` rebuilt every 5s tick.
- **Pull loop: fixed.** `PullProgress.pull_id`/`.model` are now `Arc<str>` (constructed once in `pull_model`); the per-line `.clone()`s are refcount bumps instead of fresh heap allocations + copies. A pull can emit hundreds of NDJSON lines, so this was the one genuinely hot, genuinely invariant part of this item.
- **Health loops: audit claim didn't survive verification, not fixed.** Re-read the surrounding code: `port` (in `spawn_adaptive_pathway_health_loop`) and `ollama_base_url` (inside `compute_status`, called from `spawn_health_loop`) are both re-read from `AppState.config` **fresh on every tick**, not hoisted-then-stale — and deliberately so, since both are user-editable in Settings at runtime. Caching them at loop start would mean an edited Ollama URL or Adaptive Pathway port silently keeps being probed at the *old* address until the app restarts — a real correctness regression — to save one `String::clone()`/`format!()` (tens of bytes) every 5 seconds. Left as-is.

---

## Frontend (TypeScript / React)

### 7. 🟠 Forced synchronous layout on every keystroke in the composer
- **Where:** `src/components/chat/Composer.tsx:148-149` — `onChange` sets `height='auto'` then reads `scrollHeight` (forces layout) then sets height again, per keystroke (verified).
- **Why real:** each keystroke triggers a layout+repaint before React's own render; perceptible typing latency on slower machines / long drafts.
- **Fix:** debounce the resize with `requestAnimationFrame`, or adopt CSS `field-sizing: content` and drop the JS measurement entirely.

### 8. ✅ 🟡 `pointermove` drag-reorder handler not throttled
- **Where:** `src/components/sessions/SessionList.tsx` — `window.addEventListener('pointermove', onMove)`.
- **Why real:** fires 60+×/sec during drag, each call re-rendering on `setDragOverFolder`; janky/rubber-banding with long session lists.
- **Fixed:** `onMove` now just stashes the latest `{x, y}` and schedules a `requestAnimationFrame`; the actual hit-test (`elementFromPoint`) and `setDragOverFolder`/drag-threshold check run at most once per frame, in a new `processMove`. `onUp` cancels any pending rAF. Drag-to-assign-folder behavior unchanged (verified via `tsc`/`eslint`, not manually re-tested in a live drag).

### 9. ✅ 🟡 `JSON.stringify(v, null, 2)` on large tool output in the render path
- **Where:** `src/components/chat/ToolCallCard.tsx` (params/result rendering).
- **Why real:** a large shell/file-read result stringifies on the main thread during render, dropping a frame when the card mounts/updates. The card is a `<details>`, so this ran even while collapsed — every one of a turn's tool cards, on every unrelated re-render (e.g. a streaming reasoning token), not just the one the user expanded.
- **Fixed:** `ToolCallCard` now tracks its own `open` state (via `<details onToggle>`) and only calls `stringify()` (memoized with `useMemo`) when `open` is true; collapsed cards render an empty body and do zero stringify work. Also wrapped the component in `React.memo`. Default-collapsed behavior is unchanged (no `open`/`defaultOpen` prop was set anywhere before this change).

### 10. ⚪ Inline handler closures defeat `React.memo` on `MessageItem`
- **Where:** `src/components/chat/MessageItem.tsx` — action `onClick` closures recreate each render.
- **Why real (minor):** the `memo` still guards unchanged messages, but a streaming message re-creates several closures per render → minor GC pressure. Cosmetic.
- **Fix:** `useCallback` the handlers if this ever shows up in profiling. Low value.

### 11. ⚪ `useProgressStage` churns a timer per reasoning delta
- **Where:** `src/components/chat/useProgressStage.ts` ~32–41 — `setTimeout`+`clearTimeout` on every delta.
- **Why real (minor):** heavy reasoning bursts create/destroy many timers → minor GC churn on low-end hardware.
- **Fix:** a single interval with an elapsed check instead of per-delta timer recreation.

---

### 12. ⚪ Repeated JSON-RPC envelope construction (`json!({ "jsonrpc": "2.0", … })`)
- **Where:** `goosed/api.rs` (~4 sites) build the same JSON-RPC 2.0 envelope shape inline.
- **Why real (cosmetic):** structural duplication only — each envelope carries a unique `id`, so there's nothing to cache; zero runtime cost.
- **Fix:** a small `rpc_envelope(id, method, params)` helper for readability. Hygiene, not performance.

### 13. ⚪ Session-resume rows have no per-row loading affordance
- **Where:** `components/sessions/SessionList.tsx:391` and `RecentSessions.tsx:63` — clicking a row calls `loadSession` with no row-level spinner/disable.
- **Why real (minor):** `loadSession` already sets `busy`/`replaying` and clears the pane immediately (`chatStore.ts:1466`), so the chat area *does* show loading; and a double-click only re-fetches the same read-only conversation (wasteful, not corrupting — unlike Branch/Regenerate, which were fixed). RecentSessions also closes its popover on click, removing the re-click target.
- **Audit rated this SEVERE ("loads the session twice") — it is not.** No fork/mutation occurs; worst case is a redundant read.
- **Fix (optional):** mark the clicked row `aria-busy`/disabled until `replaying` clears, for tighter feedback. Low value.

### 14. ⚪ `panic = "abort"` means any panic in a fire-and-forget task kills the process
- **Where:** the 7–8 `spawn`ed tasks (`goosed/api.rs`, `lifecycle/mod.rs`, `commands/session.rs`) whose `JoinHandle` is dropped, under `Cargo.toml:70` `panic = "abort"`.
- **Why real (design note, not a bug):** this is the deliberate fail-fast release posture. Awaiting the `JoinHandle` would **not** change it — the abort fires at panic time, before any `.await` could observe it. In practice these tasks only panic on `Mutex` poison (impossible under abort, see below) or `expect` on effectively-infallible ops (reqwest build). So the realistic panic surface is near-zero.
- **Fix:** none unless the team wants to revisit the abort policy globally (a project-wide decision, out of scope for a bug fix).

## Rejected on verification (audit was wrong — do NOT act)

- **"Debounce session search — IPC per keystroke."** FALSE. Search is client-side: `setQuery` only updates store state and `sessionStore.filtered()` filters the in-memory array (`sessionStore.ts:63`). No IPC fires on keystroke; at most a cheap re-filter. No fix needed.
- **"50× `Mutex::lock().unwrap()` → process death" (Confidence 5).** Self-refuting under `panic = "abort"` (`Cargo.toml:70`): abort doesn't unwind, so a std `Mutex` can never be poisoned, so `.lock().unwrap()` can't panic on poison in release. See INVESTIGATE.md review notes.
- **"Cyclic dependencies (5) — actionable/critical."** Intra-crate module reference cycles are normal and harmless in Rust (single compilation unit); no compile/runtime cost. Not defects.
- **"`.catch(() => {})` swallows session-cleanup failures."** These are deliberately documented best-effort cleanups (`chatStore.ts:2005+`), not silent bugs.
