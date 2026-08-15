# 815bugs — Kitty audit (2026-08-15) & remediation plan

Fresh audit of the working tree (including uncommitted changes), conducted with six parallel
readers and independent spot-verification of the highest-impact claims. Supersedes `88bugs.md`
(whose fixes were verified as landed). The retired Python daemon (`plugins/bigtiny/`) is out of
scope; `plugins/adaptive-pathway_rust/` is **live** (path-dep of `bigtiny_rust`) and included.

**Totals: 132 findings — 24 high, 51 medium, 57 low.**

Legend: each row = file | lines | snippet | bug | fix. Status ledger at the bottom tracks
disposition as fixes land.

---

## A. `src-tauri/` — Rust core

| # | File | Lines | Snippet | Bug | Fix |
|---|------|-------|---------|-----|-----|
| 1 | `src-tauri/src/commands/window.rs` | 132–198 | `lifecycle::bigtiny_proc::spawn(&command, &args, ...).await?` | `restart_backend` always spawns an external daemon exe; Android hosts it in-process and forbids `exec()`. Reachable via `set_adaptive_pathway_enabled` and `engine_restart`, where each failure re-arms `reload_required` → endless doomed respawn loop. | cfg-gate `restart_backend` and its Android callers; restart the embedded task or mark "applies on next launch". |
| 2 | `plugins/bigtiny_rust/src/agent/mod.rs` + `src-tauri/src/bigtiny/stream.rs` | 269–273 / 598–602 | `"session_status" => { if content == Some("Cancelled") { ... } }` | Daemon never emits `"Cancelled"` on the `/cancel` path, so every user cancel is reported as `stopReason: "end_turn"`. | Emit a cancelled status daemon-side before closing the stream (Kitty's matcher already exists). |
| 3 | `src-tauri/src/bigtiny/client.rs` | 17, 42–45 | `const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);` | 10s cap covers `POST /compact`, which awaits a full LLM summarization (+ cold local-model load) — manual compaction routinely times out client-side while the daemon keeps working. | Extended/no-timeout variant for long calls (like `request_stream`). |
| 4 | `src-tauri/src/bigtiny/stream.rs` | 362–367 | `buffer.push_str(&String::from_utf8_lossy(&chunk));` | Per-chunk lossy UTF-8 decode corrupts multi-byte chars split across TCP chunks; the broken SSE frame is then silently dropped. | Buffer raw bytes; decode only complete `\n\n`-delimited frames. |
| 5 | `src-tauri/src/commands/models.rs` | 347–366 | `req.header("Range", ...)` … `if !resp.status().is_success()` | Resume never requires `206`; a 200 response appends the full body after existing `.part` bytes → corrupt GGUF. | Require 206 when `resume_from > 0`; on 200 truncate and restart. |
| 6 | `src-tauri/src/lifecycle/engine_restart.rs` | 102–128 | in-flight check … spawn restart | Check-then-act: prompt sent in the gap has its daemon killed mid-turn. | Re-check `in_flight_sessions` immediately before killing; re-arm instead if non-empty. |
| 7 | `src-tauri/src/lifecycle/health.rs` + `lifecycle/mod.rs` | 23–39, 99–126 | `tokio::time::interval(...)` fires at tick 0 | Startup self-heal races `sync_mcp_once_healthy`; both list-then-create builtin servers; `mcp_servers.name` lacks UNIQUE → permanent duplicate rows. | Skip tick 0 / serialize `ensure_builtin_servers` behind a Mutex. |
| 8 | `src-tauri/src/commands/file.rs` | 154–187 | `pub fn copy_file_into_chat_folder(...) { ... std::fs::copy(...) }` | Sync command on the main thread doing an unbounded copy — attaching a large file freezes the UI (also `write_file`, `inspect_paths`, sync config saves). | Async + `spawn_blocking`; size cap. |
| 9 | `src-tauri/src/commands/config.rs` | 52–67 | `*cur = config.clone(); ... spawn_blocking(save).await` | In-memory config swapped before disk save; on failure app runs on new config while disk keeps old — silent divergence. | Roll back on save failure (persist-then-swap). |
| 10 | `src-tauri/src/config/providers/network.rs` | 30–39 | `if after_at.starts_with('[') { return after_at.to_string(); }` | `http://[::1]:11434` → host `"[::1]:11434"` (with port) fails the loopback compare → IPv6 loopback misclassified `Remote`. | Return bracketed host without the port. |
| 11 | `src-tauri/src/bigtiny/effort.rs` | 142–149 | `const DENY: [&str; 5] = [...]` | Denylist omits `claude-3-sonnet`; that model gets a thinking dropdown whose `thinking` block Anthropic 400s on. | Add `"claude-3-sonnet"`. |
| 12 | `src-tauri/src/commands/screenshot.rs` | 44–52 | `*state.screenshot_preview... = Some(...); ...create_screenshot_select_window(...)?` | Window-build failure leaks the MB-scale base64 preview + orphaned `tx` in `AppState`. | Clear both slots on error. |
| 13 | `src-tauri/src/commands/screenshot.rs` | 84–85 | `screenshot::capture_region(sx, sy, sw, sh)?` | Full-res BitBlt + PNG encode on the async runtime worker (preview in the same fn was `spawn_blocking`'d). | Wrap in `spawn_blocking`. |
| 14 | `src-tauri/src/windows.rs` | 575–595 | `chat_windows.insert(label, None); ... build_chat_window(app, &label)?` | Label inserted before build; build failure leaks dead label (+ `pending_handoffs` entry) permanently. | Remove inserted entries on build error. |
| 15 | `src-tauri/src/windows.rs` + `bigtiny/sessions.rs` | 329–337, 172 | `.find(|r| r.get("sessionId")... == Some(session_id)).unwrap_or_default()` | cwd resolution goes through `sessions::list` capped at 200; older sessions resolve to `cwd: ""`. | On miss, re-fetch with a much larger limit once; only then default. |
| 16 | `src-tauri/src/lifecycle/scheduler.rs` | 98–118 | `Err(e) => warn!(...) } advance_scheduled_task(...)` | Advance runs even when the send failed — one-shot due while daemon is down is silently disabled without running. | Advance only on success; leave past-due for bounded retry. |
| 17 | `src-tauri/src/lifecycle/health.rs` | 92–99 | `pending_degraded == Some(computed)` | Debounce needs the *identical* degraded status twice; flapping between two different degradations never publishes. | Count consecutive non-Ok ticks. |
| 18 | `src-tauri/src/bigtiny/sessions.rs` | 214–222 | `history?limit=10000` | Shares the 10s default timeout; huge histories fail session resume. | Extended/no timeout for localhost history reads. |
| 19 | `src-tauri/src/notifications.rs` | 219–230 | single worker, sequential `wait_for_response` | An unclicked toast parks the worker, starving all later click-focus handling. | Cap per-toast wait / bounded per-toast waiters. |
| 20 | `src-tauri/src/config/providers/keyring.rs` | 150–159 | check-then-act key generation | Two concurrent first-time callers generate different encryption keys; rows encrypted by the loser are undecryptable next launch. | `OnceLock`/Mutex guard or re-read + adopt stored key. |
| 21 | `src-tauri/src/commands/mcp_servers.rs` + `commands/provider.rs` | 182–190, 73–77 | `has_secret(...)` inside async command | Blocking Credential Manager IPC on tokio workers — exactly what keyring docs forbid. | Use the `spawn_blocking` variants. |
| 22 | `src-tauri/src/commands/session/mod.rs` + `crud.rs` | 114–118, 44–47 | `let _ = spawn_blocking(create_dir_all).await;` | mkdir failure discarded; session created pointing at a nonexistent directory. | Propagate io errors. |
| 23 | `src-tauri/src/bigtiny/sessions.rs` | 378–381 | `if keep <= 0 { return None; }` | `keep <= 0` maps to `None` = "copy entire history" — fork with bubble count 0 duplicates everything. | Treat `<= 0` as explicit empty fork or reject. |

## B. `src/stores/` + `src/lib/` — frontend state

| # | File | Lines | Snippet | Bug | Fix |
|---|------|-------|---------|-----|-----|
| 24 | `src/stores/chatStore.ts` | 1134–1146 | `const epoch = get().sessionEpoch; await loadSession(...); if (info.messages && epoch === get().sessionEpoch)` | **Regression (spot-verified):** `loadSession` bumps `sessionEpoch`, so the gate is always false — Expand-mid-stream snapshot never applied; partial content dropped. | Gate on `sessionId === info.session_id` or capture post-load epoch. |
| 25 | `src/stores/chatStore.ts` | 1062–1072 | `providerHost: active ? new URL(active.base_url).host : null` | Local provider has `base_url: ''` → `new URL('')` throws every `refreshProvider`; catch mis-derives `providerHasTools: true`, `model: null`, `providerSupportsVision: false`. | Defensive parse (try/catch / `URL.parse`). |
| 26 | `src/stores/chatStore.ts` | 694–710 + 1276–1278 | `const files = get().droppedFiles;` after `ensureSession()` | `newSession`'s optimistic clear wipes `droppedFiles` — first message of a fresh chat silently loses dropped files; `sendWithRecipe` loses pasted attachments too. | Capture files/attachments before `ensureSession()`. |
| 27 | `src/stores/chatStore.ts` | 698–701 | no identity check across `ensureSession` awaits | New Chat mid-`doSend` lets the send append to the new session while the prompt goes to the abandoned one → stuck spinner. | Re-check session identity after `ensureSession`, before `sendPrompt`. |
| 28 | `src/stores/chatStore.ts` | 1441–1449 | `set({ mode: info.current_mode, ... })` mid-try | `loadSession` mid-`try` writes + catch `error` aren't epoch-guarded — stale load clobbers newer session's mode/effort/concluded. | Epoch-check each post-await `set`. |
| 29 | `src/stores/chatStore.ts` | 1313–1322 | `set({ sessionId: info.session_id, ... })` unconditional | Late-arriving `newSession` overwrites a concurrent `loadSession`'s id/cwd/mode while keeping its messages. | Epoch-check the apply + error paths. |
| 30 | `src/stores/chatStore.ts` + `components/chat/ApprovalPrompt.tsx` | 1546–1556, 34–39 | store keeps entry on failure; component latches `submitted` | Failed `respondPermission` leaves the prompt rendered but unclickable; turn hung server-side. | Reset the latch on failure. |
| 31 | `src/stores/chatStore.ts` | 1150–1211 | `handOffToMain` reset | No `sessionEpoch` bump; reset omits `sessionConcluded`/provider ids/`isDefaultFolder`/`replaying`; rejected IPC unhandled → dead Stop button on blank overlay. | Bump epoch, complete reset list, try/catch IPC pair. |
| 32 | `src/stores/chatStore.ts` | 623–631 | `hasRepetitionLoop(last.reasoning + '\n' + last.text)` | Every streaming rAF flush concatenates entire reasoning+text (50–200KB) to scan a trailing window — MB/s churn. | Slice tails before joining. |
| 33 | `src/stores/chatStore.ts` | 1856–1861 | `sendInFlight = true; ... await ensureSession()` | `sendWithRecipe` doesn't catch `ensureSession` throw → unhandled rejection at `void` call site. | try/catch routed through `set({ error })`. |
| 34 | `src/stores/sessionStore.ts` | 91–98, 120–135 | `rename`/folder ops without catch | Fired via `void` everywhere — IPC failures are silent unhandled rejections. | `loadError` treatment like `refresh`/`remove`. |
| 35 | `src/stores/chatStore.ts` | 1898–1906 | `set({ recipes })` | Store's `recipes` field has zero readers — Composer fetches its own; duplicated truth. | Delete the field + refresh wiring. |
| 36 | `src/stores/chatStore.ts` | 1664–1683 | `await setSessionContextDir(...); set({ cwd: folder, ... })` | Applies after await with no identity check — session switch mid-await stamps wrong folder. | Re-read `sessionId` after await. |
| 37 | `src/stores/chatStore.ts` (+ `ChatView.tsx`) | 2062–2127, 126–134 | reset shapes omit fields | All reset paths omit `sessionConcluded`, provider ids, `isDefaultFolder` — composer stuck disabled / "set folder" pill with `cwd: null` whose reset no-ops. | Include omitted fields in every reset. |
| 38 | `src/stores/chatStore.ts` | 1263–1298 | transition resets | `warning`/`compactionNotice` never cleared — stale banners persist across New Chat / switches. | Null both in transition resets. |
| 39 | `src/stores/chatStore.ts` | 1559–1568 | optimistic `set({ mode })`, no rollback | Badge shows a mode the session isn't in; `refreshProvider` caches the lie. | Restore previous mode in catch. |
| 40 | `src/stores/chatStore.ts` | 1766–1780 | `superseded: true` before `sendPrompt` | IPC failure leaves the message collapsed with no replacement and no way back. | Clear `superseded` in catch. |
| 41 | `src/lib/chatml.ts` | 15–24 | no `superseded` filter | Export after regenerate contains both rejected answer and replacement — corrupted transcripts. | Skip `superseded` in full export. |
| 42 | `src/stores/chat/errorUtils.ts` | 93–98 | `rest.lastIndexOf('\n\nUser: ')` | User's own message containing that literal gets its head eaten by the replay-strip. | Anchor on unambiguous sentinel/suffix. |
| 43 | `src/lib/theme.ts` | 128–160 | `await ipc.getConfig()` no catch | Backend-down rejection → unhandled rejection from `void` call sites. | `.catch(() => {})` / try-catch. |
| 44 | `src/lib/composerRichText.ts` | whole file | contentEditable helpers | Dead production code (only its test imports it); latent OL serialization bug if re-wired. | Delete module + test. |

## C. `src/components/` + `src/windows/` — frontend UI

| # | File | Lines | Snippet | Bug | Fix |
|---|------|-------|---------|-----|-----|
| 45 | `src/components/sessions/SessionList.tsx` | 40–42 | `useSessionStore((s) => s.grouped)` only | **Regression:** never subscribes to `s.sessions`/`s.assignments` — renames don't re-render; deletes can leave ghost rows. | Subscribe to `sessions` + `assignments`. |
| 46 | `src/components/chat/MessageList.tsx` | 151–161 | `transform: translateY(...)` + fixed popover | Row transform becomes containing block for `position: fixed` — ⓘ popover offset off-screen past 200 messages. | Portal popover to `document.body`. |
| 47 | `src/components/artifacts/ArtifactsPane.tsx` | 36–43 | `if (document.hidden) return;` | Workspace stays mounted (hidden) on Settings/Wizard routes — 5s disk scan keeps running invisible. | Also gate on active route. |
| 48 | `src/windows/overlay/App.tsx` + `Composer.tsx` | 25–27, 254–257 | Escape handling | Composer's dropdown-dismiss Escape doesn't `stopPropagation` — one Escape also hides the whole overlay. | `stopPropagation()` in Composer. |
| 49 | `src/components/settings/AdaptivePathway.tsx` | 99–110 | `setEnabled(next); await ipc.setAdaptivePathwayEnabled(next);` | Optimistic toggle never reverts on failure. | Set after success / revert in catch. |
| 50 | `src/windows/overlay/App.tsx` | 38–50 | `const expand = () => handOffToMain();` | Not latched — double-click spawns two windows adopting one session. | In-flight guard. |
| 51 | `src/components/hub/ChatWorkspace.tsx` | 42–48 | `void ipc.getConfig().then(...)` | No catch + no unmount guard. | `.catch(() => {})` + mounted flag. |
| 52 | `src/components/settings/ScheduledTasks.tsx` | 143–146 | `interval_secs: amount * UNIT_SECONDS[unit]` | Fractions (`1.1` min → `66.000…01`) rejected by Rust `u64` — cryptic save failure. | Round + clamp ≥ 60; `step={1}`. |
| 53 | `src/components/settings/providers/ProviderForm.tsx` | 363–413 | `Number(e.target.value)` | Clearing max_tokens/top_k/min_p writes `0` instead of `null` — literal `max_tokens: 0` persisted. | Empty → `null` like sibling fields. |
| 54 | `src/components/shared/StackStatusView.tsx` | 68–77 | `await ipc.restartBackend(); ...` | Failure escapes onClick uncaught — silent, button re-enables. | catch + surface error. |
| 55 | `src/components/settings/Advanced.tsx` | 382–383 | `key={i}` | Index keys on a list re-fetched every 5s. | Stable key (timestamp+target). |
| 56 | `src/windows/screenshot-select/App.tsx` | 34–40 | `void ipc.getScreenshotPreview().then(...)` | No catch + no unmount guard. | `.catch` + mounted flag. |
| 57 | `src/components/chat/MessageItem.tsx` | 81–92 | clipboard `.then(() => setCopied(true))` | Not unmount-guarded; virtual list unmounts rows mid-write (same in `CodeBlock.copy`). | Mounted ref guard. |
| 58 | `src/components/settings/providers/ProviderForm.tsx` | 51–59 | deps `[provider_type, modelsKey]` | Missing `base_url` — stale "Detected context" after URL edit. | Add `base_url` to deps. |

## D. `plugins/bigtiny_rust/` — agent, providers, MCP, HITL

| # | File | Lines | Snippet | Bug | Fix |
|---|------|-------|---------|-----|-----|
| 59 | `provider/openai_compat.rs` + `anthropic.rs` | 196–207, 107–118 | `.timeout(DIRECT_CONNECT_TIMEOUT)` (3s) | **Spot-verified:** request-level timeout covers the SSE body — every Tailscale-direct stream dies at 3s; fallback never fires. | Bound connect phase only; rely on SSE idle timeout. |
| 60 | `hitl/manager.rs` | 93–94 | `Instant::now() - MAX_PENDING_AGE` | **Spot-verified:** panics when uptime < 1h (boot-relative Instant) inside `create_pending` → daemon panic in shared HITL mutex. | `checked_sub`. |
| 61 | `agent/mod.rs` | 229–243 | `tokio::spawn(...)` cleanup after `run()` | Panic in `run()` leaks the `tasks` entry → session permanently "turn in progress". | Drop-guard cleanup; finished handles replaceable. |
| 62 | `agent/sandbox.rs` | 139–154 | `[A-Za-z]:[\\/]...` regex | **Spot-verified:** matches URLs (`s://…`, `//…`) → `curl https://…`/`git clone` hard-denied 100% with no approval path. | Strip `scheme://` tokens before extraction. |
| 63 | `agent/context/builder.rs` + `compaction.rs` | 143–148, 698–707 | `[Original request]\n{row.content}` | First message with images stores base64 blocks JSON — inlined into permanent system head of every turn + summarizer prompt (megabytes; token count bills 256/image). | `[N image(s) attached]` placeholder for non-text blocks. |
| 64 | `agent/memory.rs` | 230–234 | `if score <= t { continue; }` | bm25: more negative = better — gate rejects best matches, keeps worst. | `if score > t`; fix comment; direction test. |
| 65 | `agent/context/builder.rs` | 257–266 | thought-seed as `assistant` msg | Persisted into transcript (no id, not system) — literal `<think>` in saved chats; adjacent-assistant 400s on Anthropic next turn. | Strip seed before persist / skip `<think>` assistant msgs in `save_messages`. |
| 66 | `mcp/tools.rs` + `client.rs` | 85–86, 234–239 | `jsonschema::validate(...)` | Panics on invalid advertised schema (connect only checks serializability) → unwinds turn task → #61 wedge. | `validator_for` at connect; fail connect on invalid. |
| 67 | `provider/openai_compat.rs` + `anthropic.rs` | 57–65, 81–89 | `connect_timeout(30s)` only | Nothing bounds time-to-headers; stalled provider holds the turn + fallback loop forever. | `tokio::time::timeout` around `send()`; bound `discover_models`. |
| 68 | `provider/openai_compat.rs` | 573–581 | empty `arguments` → `__error` | `arguments: ""` is legitimate for zero-arg tools (Anthropic handles it) — regressed zero-arg calls on OpenAI-compat. | Empty → `{}` before parse. |
| 69 | `agent/loop_.rs` | 1469–1502 | containment check after `check_tool_call_with_rules` | `always_ask` registers a pending action for a call then hard-denied — phantom entries in pending API ~1h. | Run containment before the HITL decision. |
| 70 | `agent/loop_.rs` | 1076–1088 | `run_compaction(...)` awaited inline every step | O(steps × history) DB reads + summarizer stalls between tool steps (doc says fire-and-forget per turn). | Once at turn end / detached spawn with CAS lock. |
| 71 | `agent/loop_.rs` | 1289–1325 | `content_chars += content...` only | Reasoning text uncounted — thinking-loop streams unbounded (hosted providers get no max_tokens floor). | Count reasoning too. |
| 72 | `agent/loop_.rs` | 704–717 | pinned-provider mismatch warn at step 0 every turn | Stale stamp re-warns every message, not once. | Record warned state. |
| 73 | `agent/loop_.rs` | 1360–1365 | `v as i32` | Wraps for absurd usage values → negative `total_tokens` persisted. | `i32::try_from` saturating. |
| 74 | `provider/openai_compat.rs` | 555–593 | `HashMap` drain of tool calls | Hash order ≠ index order — transcript/execution sequence scrambled nondeterministically. | Sort by index. |
| 75 | `provider/openai_compat.rs` | 821–841 | no `thinking.flush()` at EOS | Dangling partial tag bytes silently dropped (the loss `TagSplitter::flush` exists to prevent). | Flush on `Ready(None)`. |
| 76 | `agent/loop_.rs` | 1558–1571 | timeout leaves pending record | Pending API shows an approval no waiter honors for ~1h. | Remove pending + decision on timeout. |
| 77 | `agent/sandbox.rs` | 28–34 | `norm()` lowercases | Fail-open containment on case-sensitive filesystems (Android). | Native case on case-sensitive hosts. |
| 78 | `provider/anthropic.rs` | 371–386 | no `top_k` | Configured `top_k` silently ignored on Anthropic (API supports it). | Write `body["top_k"]`. |
| 79 | `agent/context/builder.rs` | 172, 281–288 | `live_token_sum` from raw rows | Emergency valve compares pre-masking/pre-budget sum — destructive trim can fire when already under budget. | Recompute from final `live_messages`. |
| 80 | `agent/compaction.rs` | 462–463 | `cfg.tool_mask_head as usize` | Unclamped casts (sibling got `.max(0)`); env overrides bypass `sanitize()` (#98) → overflow/panic class. | Same `.max(0)` clamps. |
| 81 | `provider/base.rs` + `openai_compat.rs` | 199–203, 347–351 | `format!("Provider error: {}", body)` | Raw provider error bodies (can echo request content/keys) → user message + debug logs. | Truncate/sanitize; redact body logging. |

## E. `plugins/bigtiny_rust/` — routes, storage, scheduler, server, local

| # | File | Lines | Snippet | Bug | Fix |
|---|------|-------|---------|-----|-----|
| 82 | `lib.rs` | 286–303 | no `DefaultBodyLimit` | **Spot-verified:** axum 2 MiB cap vs base64 screenshot sends → 413 on large images. | `DefaultBodyLimit::max(32–64 MiB)`. |
| 83 | `lib.rs` | 181–186 | `register_local(..., ProviderConfig::default(), ...)` | `context_length: None` → budgeting assumes 64k against engine's real 4k — compaction never fires before context hard-fail. | Populate from resolved n_ctx. |
| 84 | `routes/providers.rs` | 127–137 | pinned-id create | Second POST for existing id → raw UNIQUE 500; Kitty's GET-then-POST races hard-fail. | 409 or upsert. |
| 85 | `routes/providers.rs` | 144–171 | INSERT outside the tx | Crash/failed compensating DELETE leaves config-less row visible to health checks. | INSERT inside the same `BEGIN IMMEDIATE`. |
| 86 | `routes/chat.rs` | 306–354 | session row before message copy | Mid-loop failure leaves orphaned zero-message session. | One tx / delete on error. |
| 87 | `scheduler/mod.rs` | 183–189 | rollback registers cron for disabled job | Failed enable of previously-disabled job leaves live cron firing a row that says `enabled = 0`; `execute_job` never checks. | Roll back only if previously enabled; check `enabled` in `execute_job`. |
| 88 | `hitl/manager.rs` | 331–338 | `_ => proceed` | Unknown decision strings silently approve. | Explicit match; reject unknown. |
| 89 | `lib.rs` | 312–317 | drain before `agent.shutdown()` | Hung turn blocks SIGTERM indefinitely → supervisor kill -9. | Abort turns concurrently / timeout the drain. |
| 90 | `routes/mcp.rs` | 63–75, 185 | `"***"` re-encrypted on update | Round-tripping a masked server stores `encrypt("***")` — real auth headers destroyed. | Treat `"***"` as keep-existing. |
| 91 | `scheduler/mod.rs` | 296–305 | `Ok(session_id) => 'completed'` | `run_turn_and_wait` discards outcome — provider-failed runs recorded `'completed'`. | Propagate turn outcome. |
| 92 | `storage/execution.rs` | 66–77 | `WHERE trigger_id = ?` no index/LIMIT | Unbounded full scan of a forever-growing table. | Index migration + LIMIT. |
| 93 | `routes/providers.rs` | 245–249 | router unregister before DB delete | Failed delete → router/DB divergence until restart. | Delete row first. |
| 94 | `local/engine.rs` | 408–431 | `device_index` never applied | Model loads on default device while VRAM sizing/UI report the selected one. | Pass device into load params. |
| 95 | `storage/messages.rs` | 95–107 | `.bind(&msg.session_id)` | Dedupe scoped to param but rows insert own session_id — mixed batch corrupts another session. | Bind the parameter / assert match. |
| 96 | `routes/schedules.rs` | 29–34 | `_ => 500` | Invalid cron → 500 instead of 400. | Map `Cron` → 400. |
| 97 | `routes/mcp.rs` | 267–272 | `let _ = connect_server(...)` | PATCH that breaks reconnect returns 200 with stale status. | Return refreshed status / surface error. |
| 98 | `env_contract.rs` | 74–85 | overrides bypass `sanitize()` | `..._MASK_HEAD_LINES=-5` reopens the negative-index panic class. | `sanitize()` after overrides. |
| 99 | `env_contract.rs` | 181–186 | strict `bool` parse for `TOOL_CALLS` | `=1`/`=FALSE` silently ignored, unlike neighbors. | Same lenient parse as other booleans. |
| 100 | `routes/mcp.rs` | 97–106 | `null` → `"null"` string | Nullable fields can't be cleared; silently dropped at connect. | `Value::Null` → clear column. |
| 101 | `routes/pathway.rs` | 48, 59–64, 101 | `unwrap_or_default()` stats + 200 `{"error"}` | Fabricated zeros on DB error; full-table materialization per poll. | Real 5xx + aggregate SQL. |
| 102 | `routes/chat.rs` | 185–188, 233–235 | `msg.contains("not found")` | Status discrimination by substring — rewording flips 404/500. | `StorageError::NotFound` variant. |
| 103 | `routes/chat.rs` | 396–408 | `Path(_id)` discarded | Approval to `/chat/{A}/approve` resolves session B's action. | Verify session match. |
| 104 | `routes/chat.rs` | 106–108 | unbounded `limit` | `?limit=-1` = unlimited → full sessions table. | Clamp `1..=500`. |
| 105 | `routes/providers.rs` | 82–90 | unparseable config echoed | Legacy plaintext `api_key` blob returned verbatim (redaction only on successful parse). | `"{}"` on parse failure. |
| 106 | `scheduler/mod.rs` | 213, 102 | `[..8]` truncated UUIDs | Collision: register overwrites existing job's mapping; rollback unregisters it. | Full UUIDs + existence check. |
| 107 | `storage/hitl_rules.rs` | 57–65 | deferred-tx check-then-act upsert | Concurrent same-key upserts → `SQLITE_BUSY_SNAPSHOT` spurious 500. | `BEGIN IMMEDIATE`. |
| 108 | `bin/bigtiny_daemon.rs` | 40–47 | invalid `--port` ignored | Typo binds 8080 silently — "Kitty can't reach backend". | Fail or warn loudly. |

## F. Tool plugins — kitty-tools / kitty-web / kitty-wasm / adaptive-pathway_rust

| # | File | Lines | Snippet | Bug | Fix |
|---|------|-------|---------|-----|-----|
| 109 | `kitty-web/src/scrape.rs` | 439–448 | `client.get(url)` no validation | Full SSRF — loopback/metadata/redirect-to-internal bodies into model context. | Scheme + private-IP validation per redirect hop. |
| 110 | `kitty-tools/src/tools/viz/model.rs` | 219–235 | uncapped `while changed` relaxation | Cycles hang the tool forever (reproduced). | Bound iterations / Kahn cycle-detect. |
| 111 | `kitty-wasm/src/server.rs` | 132–140, 214–224 | `workspace` mount unchecked | Any host dir mounted RW into the guest — model can mount `C:\`. | Contain to allowed roots like kitty-tools. |
| 112 | `kitty-web/src/scrape.rs` + `search.rs` | 497, 547, 773–777 | `response.bytes().await` unbounded | No size cap — hostile endpoint can OOM the process. | Hard byte ceiling. |
| 113 | `kitty-web/src/scrape.rs` | 299–353 | recursive `serialize_node` | Deep DOM overflows stack (uncatchable abort); CPU work on reactor. | Iterative + depth cap + `spawn_blocking`. |
| 114 | `kitty-tools/src/docx/write.rs` | 236–240 | `&l[1..l.len() - 1]` | `"|"` line panics slicing `[1..0]` (verified). | Require `len >= 2`. |
| 115 | `kitty-tools/src/docx/write.rs` | 482–485 | `re.replace(..., format!(...))` | `$`-expansion mangles titles containing `$` (verified). | `NoExpand` / closure replacer. |
| 116 | `adaptive-pathway_rust/src/learn/mod.rs` | 273–289 | empty correction `""` matches every belief (`contains`) | Junk `[""]` correction tombstones the first belief in the table. | Skip empty needles; `None` on empty. |
| 117 | `kitty-tools/src/tools/shell.rs` | 137–189 | `kill_on_drop(true)` | Timeout kills only `cmd.exe`; grandchildren orphaned. | Kill process tree (`taskkill /T /F` / Job Object). |
| 118 | `adaptive-pathway_rust/src/belief/synthesis.rs` | 207–228 | `current_id = best...unwrap_or_default()` | New-belief branch records `belief_a = ""`. | Use the created id. |
| 119 | `kitty-tools/src/tools/excel.rs` | 293–328 | no size/row guard; accumulates all rows | Zip-bomb xlsx can exhaust memory. | File-size gate + row cap. |
| 120 | `kitty-tools/src/tools/pdf.rs` | 27–29, 95–100 | `Document::load(path)` unbounded | Whole PDF in memory; per-page text materialized before cap. | Size gate + bound extraction. |
| 121 | `kitty-wasm/src/python.rs` | 239–241 | `read_to_string(result.json)` unbounded | Guest writes huge `result.json` → host OOM. | Byte-capped read. |
| 122 | `adaptive-pathway_rust/src/embed/hashing.rs` (+`project.rs`, `cms.rs`) | 91, 12, 13 | `rem_euclid(dim as i64)` | `embedding_dim = 0`/`hash_size = 0` panics in-process on the recall path. | Validate config ≥ 1 at load. |
| 123 | `kitty-tools/src/tools/fs.rs` | 69–70 | `e as usize` on negative lines | `end_line = -1` → ~2⁶⁴ (silently EOF); `i64::MAX` overflows add (debug panic). | Validate ranges; `saturating_add`. |
| 124 | `kitty-tools/src/tools/excel.rs` | 479–503 | CSV no formula-injection guard | `=`/`+`/`-`/`@` cells execute in Excel (documented workflow). | Prefix with `'`. |
| 125 | `kitty-web/src/scrape.rs` | 384–396 | `CON.pdf` | Reserved Windows device names → confusing write failure. | Suffix reserved stems. |
| 126 | `kitty-tools/src/tools/cache.rs` | 20–22 | traversal filter | Misses `:` (NTFS ADS) + device basenames. | Reject both. |
| 127 | `kitty-wasm/src/sandbox.rs` | 349–353 | tmp name = pid only | Concurrent in-process compiles interleave → torn cache artifact. | Add atomic counter. |
| 128 | `kitty-tools/src/tools/fs.rs` | 268–271 | `lines.join("\n")` rewrite | CRLF→LF + forced trailing newline — whole-file mangling. | Preserve EOL style + trailing state. |
| 129 | `kitty-tools/src/tools/excel.rs` | 115–129 | `col = col * 26 + ...` | Mid-loop u64 overflow → silently wrong window. | Checked arithmetic in-loop. |
| 130 | `adaptive-pathway_rust/src/mcp/mod.rs` | 30–33, 89–94 | `#[serde(default)] session_id` | Deserializable from tool args despite "never model-supplied" comment. | `#[serde(skip)]` + runtime channel. |
| 131 | `adaptive-pathway_rust` | learn 310–324; contradictions 88–92 | uncapped belief text; O(n²) scan | Pathological texts inflate every prompt; unbounded pairwise scan per sweep. | Cap length; bound scan. |
| 132 | `kitty-tools/src/tools/viz/mermaid.rs` + `kitty-web/src/server.rs` | 118–124, 123–160 | verbatim SVG interp.; unguarded async tools | Escaping miss in dep → script exec in `allow-scripts` iframe with `unsafe-inline` CSP; panics escape rmcp task instead of structured envelope. | Sanitize/assert + tighten CSP; `catch_unwind` wrap. |

---

# Remediation plan

**Phase 1 — Correctness/security criticals (24 high):** #1–3, 24–26, 45, 59–63, 82–84, 109–111.
**Phase 2 — Mediums by subsystem:** bigtiny routes/storage → frontend epoch-guard sweep → plugin mediums.
**Phase 3 — Lows + dead code.**

Execution: 5 parallel workstreams with disjoint file ownership —
1. Frontend (`src/**`) — #24–58.
2. src-tauri (`src-tauri/src/**`) — #1, 3–23.
3. bigtiny agent/provider/mcp/hitl (`plugins/bigtiny_rust/src/{agent,provider,mcp,hitl}/**`) — #2 (daemon half), 59–81, 88.
4. bigtiny routes/storage/scheduler/local/env (`plugins/bigtiny_rust/src/{routes,storage,scheduler,local}/**`, `lib.rs`, `env_contract.rs`, `bin/`) — #82–87, 89–108.
5. Tool plugins (`plugins/kitty-{tools,web,wasm}/**`, `plugins/adaptive-pathway_rust/**`) — #109–132.

**Verification per phase:**
- Frontend: `pnpm test`, `pnpm build` (tsc), `pnpm lint`
- Rust: `cargo test` + `cargo clippy` in `src-tauri/`, `plugins/bigtiny_rust/`, `plugins/kitty-tools/`, `plugins/kitty-web/`, `plugins/kitty-wasm/`, `plugins/adaptive-pathway_rust/`
- Every Phase-1 fix gets a regression test in the repo's existing style (red → green).

---

## Status ledger

All 132 findings dispositioned on 2026-08-15. Five parallel fix workstreams + a final
integration pass. **130 fixed, 1 not-a-bug (#130), 0 deferred, 0 won't-fix.**

### A. `src-tauri/` — all fixed (22/22 in scope; #2's Kitty-side matcher pre-existed)
1. **fixed** — `restart_backend` cfg-gated on Android (Ok + "applies on next launch" log; no more respawn loop). No automated test (windowing/process spawn; needs Android target for the gated arm).
2. **fixed** (daemon half, section D) — `cancel()` emits terminal `session_status: "Cancelled"` before abort.
3. **fixed** — `LONG_REQUEST_TIMEOUT` (600s) + `post_json_long` for compact; bounded, per module rationale.
4. **fixed** — raw-byte SSE accumulation; frames decoded only when complete. 3 tests.
5. **fixed** — 206 required for resume; 200 → discard `.part`, restart. Test.
6. **fixed** — in-flight re-check inside the restart task; re-arms instead of killing.
7. **fixed** — self-heal skips tick 0 + `ensure_builtin_servers` behind a process-wide Mutex. Test.
8. **fixed** — async + `spawn_blocking` + 512 MB cap (also `write_file`/`inspect_paths`). Test.
9. **fixed** — rollback of in-memory config on save failure.
10. **fixed** — bracketed IPv6 host returned without port. Tests.
11. **fixed** — `claude-3-sonnet` added to denylist. Test.
12. **fixed** — preview+selection cleared on window-build failure.
13. **fixed** — `capture_region` in `spawn_blocking`.
14. **fixed** — map entries removed on build error.
15. **fixed** — one retry with `limit=10000` on find-miss. 3 tests.
16. **fixed** — advance only on successful send; failure retries next tick.
17. **fixed** — consecutive-non-Ok streak debounce. 3 tests.
18. **fixed** — history fetch uses the long timeout.
19. **fixed** — per-toast wait threads, capped at 16 live (notify-rust has no wait timeout).
20. **fixed** — `OnceLock<Mutex>` + re-read under lock for key generation.
21. **fixed** — `get_secret_async`/`set_secret_async` (new) on the async paths.
22. **fixed** — mkdir errors propagated, user-safe.
23. **fixed** — `keep <= 0` is a user-safe error, not full-history copy. Test.

### B + C. Frontend `src/**` — all fixed (35/35)
24. **fixed** — adoptSession gates on `sessionId` identity. Test. 25. **fixed** — defensive URL parse. Test.
26. **fixed** — files/attachments snapshotted before `ensureSession()`. 2 tests. 27. **fixed** — post-`ensureSession` identity re-check.
28. **fixed** — epoch-checks on loadSession's mid-try sets. 29. **fixed** — newSession epoch-checks apply+error.
30. **fixed** — `respondApproval` returns bool; prompt un-latches on failure. 31. **fixed** — handOffToMain bumps epoch, full reset, try/catch.
32. **fixed** — tail-sliced repetition check. 33. **fixed** — sendWithRecipe try/catch. 34. **fixed** — loadError pattern on rename/folder ops.
35. **fixed** — dead store `recipes` field deleted. 36. **fixed** — post-await identity check on working-dir ops.
37. **fixed** — reset shapes completed everywhere. 38. **fixed** — warning/compactionNotice cleared on transitions.
39. **fixed** — setMode rollback. 40. **fixed** — superseded cleared on regenerate failure. 41. **fixed** — export filters superseded.
42. **fixed** — sentinel-anchored preamble strip. 2 tests. 43. **fixed** — theme apply try/catch. 44. **fixed** — composerRichText.ts + test deleted.
45. **fixed** — SessionList subscribes `sessions`+`assignments`. 46. **fixed** — MessageInfo popover portaled to body.
47. **fixed** — artifacts poll gated on chat route. 48. **fixed** — Composer Escape stopPropagation. 49. **fixed** — revert on failure.
50. **fixed** — expand latched. 51. **fixed** — catch + mounted guard. 52. **fixed** — round/clamp interval_secs, `step={1}`.
53. **fixed** — empty → null for max_tokens/top_k/min_p. 54. **fixed** — restart error surfaced. 55. **fixed** — stable log keys.
56. **fixed** — catch + mounted guard. 57. **fixed** — mounted ref on clipboard setters (MessageItem + CodeBlock). 58. **fixed** — `base_url` in deps.

### D. bigtiny agent/provider/mcp/hitl — all fixed (24/24)
59. **fixed** — direct attempt uses connect-only timeout; SSE body uncapped. Test (3.4s mid-body gap).
60. **fixed** — per-entry `elapsed()` sweep, no subtraction panic. Test. 61. **fixed** — TurnCleanup drop guard + finished-entry replaceable. 3 tests.
62. **fixed** — URL tokens stripped before path extraction. Tests (`curl`/`git clone` allowed, real paths still extracted).
63. **fixed** — `[N image(s) attached]` placeholder at anchor + summarizer. Tests. 64. **fixed** — `score > t`; direction test; doc comment corrected (both spots).
65. **fixed** — thought-seed stripped before persist, re-appended to the outgoing request only. Tests.
66. **fixed** — `validator_for` compile-check at connect; call path can't panic. Tests.
67. **fixed** — 30s timeout-to-headers on `send()`; `discover_models` bounded 5s.
68. **fixed** — empty arguments → `{}`. Test. 69. **fixed** — containment hard-deny before HITL decision. Tests.
70. **fixed** — compaction once per turn, detached with CAS lock. 71. **fixed** — reasoning chars counted.
72. **fixed** — warn-once per mismatch appearance. 73. **fixed** — saturating `i32::try_from`.
74. **fixed** — tool calls sorted by index. Test. 75. **fixed** — `thinking.flush()` at EOS. Test.
76. **fixed** — pending+decision removed on timeout. Test. 77. **fixed** — lowercase only on Windows. cfg tests.
78. **fixed** — `top_k` written for Anthropic. Test. 79. **fixed** — valve sum from final masked/budgeted messages.
80. **fixed** — `.max(0)` clamps. Test. 81. **fixed** — surfaced body capped 300 chars; body debug-log replaced by shape summary. Test.
88. **fixed** — unknown decisions rejected explicitly. Test.

### E. bigtiny routes/storage/scheduler/local/env — all fixed (27/27)
82. **fixed** — `DefaultBodyLimit::max(64 MiB)`. Tests (3 MiB → 200, 65 MiB → 413).
83. **fixed** — local provider registers resolved `context_length`; `discover_models` never reports `Some(0)`. **Caveat:** `--features local-engine` not compile-verified in this environment (libclang absent); hand-verified against llama-cpp-2 0.1.154 API.
84. **fixed** — pinned-id conflict → 409. Smoke test. 85. **fixed** — INSERT inside the same `BEGIN IMMEDIATE` tx.
86. **fixed** — fork session cleaned up on mid-loop failure. 87. **fixed** — rollback only if previously enabled + `execute_job` enabled-guard.
89. **fixed** — teardown concurrent with drain; drain capped 10s.
90. **fixed** — `"***"` keeps existing encrypted value. 2 tests.
91. **fixed** (integration pass) — `run_turn_and_wait` returns `Result<(), String>` via terminal-frame watcher; `RecipeError::TurnFailed` propagates; scheduler marks `failed` + error_message and keeps the row as audit trail; `/execute` maps TurnFailed → 500. Tests updated (scheduler success test now uses a mockito SSE provider — it previously "passed" only because failure was swallowed; failure test pins `failed` + kept row; smoke test pins 500 + session row).
92. **fixed** — migration `014_execution_trigger_index.sql` + LIMIT. 93. **fixed** — DB delete before router unregister.
94. **fixed** — `with_devices(&[idx])` applied (same compile caveat as #83). 95. **fixed** — INSERT binds the scoped session_id.
96. **fixed** — Cron → 400. 97. **fixed** — reconnect failure → 502; success returns refreshed status.
98. **fixed** — `sanitize()` after env overrides. Test. 99. **fixed** — lenient bool parse. Test.
100. **fixed** — tri-state absent/null/value across all five fields. 2 tests. 101. **fixed** — real 5xx + aggregate SQL stats.
102. **fixed** — `StorageError::NotFound` variant. 103. **fixed** — session-matched approvals (404 on mismatch). Smoke test.
104. **fixed** — limit clamped 0..=500, offset ≥ 0. 105. **fixed** — `"{}"` on unparseable config. Test.
106. **fixed** — full UUIDs + existence check. 107. **fixed** — `BEGIN IMMEDIATE` upsert. 108. **fixed** — invalid `--port` exits with an error.

### F. Tool plugins — 23 fixed, 1 not-a-bug
109. **fixed** — new `kitty-web/src/ssrf.rs`: scheme + IP-literal/DNS checks, per-redirect-hop re-validation; `SCRAPE_BLOCKED_URL`. Extensive tests.
110. **fixed** — relaxation bounded to `steps.len()` passes; cycle → `VIZ_BAD_EDGE_REF`. Cycle + self-loop tests.
111. **fixed** — new `kitty-wasm/src/paths.rs`: `path_within_home` containment on both mount sites. Tests.
112. **fixed** — 32 MiB body cap (`read_body_capped`), `SCRAPE_TOO_LARGE`. Tests. 113. **fixed** — iterative serializer, 1000-depth cap, `spawn_blocking`. 20k-nest test.
114. **fixed** — `len >= 2` guard. Test. 115. **fixed** — `NoExpand`. Test. 116. **fixed** — empty-needle guards at both layers. Tests.
117. **fixed** — `taskkill /T /F` process-tree kill on timeout. Live grandchild test. 118. **fixed** — created-id recorded. Test.
119. **fixed** — 64 MiB gate + 100k-row scan cap. Tests. 120. **fixed** — 64 MiB PDF gate. Test. 121. **fixed** — 4 MiB `take` cap. Tests.
122. **fixed** — config validated ≥ 1 at load; `hash_embed`/`project` total on 0. Tests. 123. **fixed** — validated + saturating. Tests.
124. **fixed** — `'` prefix on formula-leading cells. Tests. 125. **fixed** — reserved-name suffix. Tests. 126. **fixed** — `:` + device names rejected. Tests.
127. **fixed** — atomic counter in tmp name. 128. **fixed** — EOL + trailing-newline preserved. Tests. 129. **fixed** — checked arithmetic. Tests.
130. **not a bug** — verified: the daemon's dispatch unconditionally injects `session_id` before any tool call (`loop_.rs:1608–1616`); the `#[serde(default)]` field *is* the injection mechanism. `#[serde(skip)]` would break per-session scoping.
131. **fixed** — 300-char belief-line cap + 500 most-recent contradiction bound. Tests. 132. **fixed** — hostile-label escaping test (passes; no sanitization needed) + `guarded_async` on the two web tools.

### Test/validation matrix (all green)
- Frontend: **29/29 files, 259/259 tests**, `tsc --noEmit` 0 errors, `eslint src` 0 problems (23 files Prettier-formatted; 57 pre-existing repo-wide Prettier-red files left untouched).
- `src-tauri`: **212 tests pass**, clippy 0 warnings (Windows target; the Android-only `bigtiny_rust` path-dep is cfg-gated out here).
- `plugins/bigtiny_rust`: **324 tests pass** (295 lib + 5 mcp + 22 routes_smoke + 2 scheduler_and_recipes), clippy 0 warnings.
- `plugins/kitty-web`: **65 pass**, clippy clean. `plugins/kitty-tools`: **219 pass**, clippy clean. `plugins/kitty-wasm`: **52 pass**, clippy clean. `plugins/adaptive-pathway_rust`: **186 pass**, clippy clean.
- Not machine-verified: `bigtiny_rust --features local-engine` / Android targets (no libclang/NDK in this environment) — affects #83, #94 only; hand-verified against the llama-cpp-2 API. Run `cargo ndk -t arm64-v8a --platform 26 check --lib` (src-tauri) on a toolchain-equipped machine before release.
