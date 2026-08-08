# 88bugs — Kitty audit findings, verification status & fixes

Audit of: `src/` (frontend), `plugins/bigtiny_rust/`, `plugins/adaptive-pathway_rust/`.

Legend for **Status**: `open` = not yet verified; `verified` = confirmed real but not fixed; `fixed` = repaired + tests pass; `won't fix` = intentionally left as-is (documented reason); `not a bug` = refuted on re-verification.

---

## A. Frontend — core store / lib logic

| ID | File | Lines | Snippet | Issue | Status |
|----|------|-------|---------|-------|--------|
| 1 | `src/stores/chatStore.ts` | 1321–1326 | `await get().newSession(); return get().sessionId!;` | `newSession()` never rethrows; on failure `sessionId` stays null and the `!` assertion lies, so `doSend` proceeds with a null session. | open |
| 2 | `src/stores/chat/loopGuards.ts` | 103–118 | `` return `${title}::${target}`; `` | Guard claims to catch alternating tools on same target, but key includes title, so alternating tools never trip the threshold. | open |
| 3 | `src/stores/chat/loopGuards.ts` | 89–97 | `text.indexOf(' response')` | Probes for literal `" response"` substring anywhere; normal prose hijacks reasoning parsing on every message. | open |
| 4 | `src/stores/chatStore.ts` | 1447–1455 | `const stillBusy = await ipc.isSessionBusy(..); return { busy: stillBusy, ... }` | Epoch check runs before await; stale finally clobbers newer session state. | open |
| 5 | `src/stores/chatStore.ts` | 1758–1770 | `promptText = `${userText}\n\n(Please reconsider...)`` | `regenerate()` drops images/attachments that the original turn had. | open |
| 6 | `src/stores/chat/approvalUtils.ts` | 51–54 | `options.find((o) => /allow/i.test(o.optionId))` | Fallback can match `allow_always`, silently persisting auto-approval. | open |
| 7 | `src/stores/chatStore.ts` | 2008–2016 | `const id = String(u.toolCallId ?? '');` | Missing toolCallId merges all calls into `toolCalls[0]`. | open |
| 8 | `src/stores/chatStore.ts` | 907–940 | failure branch restores only some fields | Restores sessionId/modeOverride but not mode/availableModes/thinkingEffort; duplicate IPC calls. | open |
| 9 | `src/stores/chatStore.ts` | 1225–1295 | `set({ ... modeOverride: null ...})` then `get().modeOverride ?? 'chat'` | Read-after-write clears mode before read; mode is always `'chat'`. | open |
| 10 | `src/stores/chatStore.ts` | 739–741 + ~1624 | `files.filter((f) => !imageFiles.includes(f))` | O(n²) double linear scan per drop. | open |
| 11 | `src/stores/chatStore.ts` | 2280–2286 | `pendingText += e.text` per delta | Unbounded string concat until rAF flush; O(n) realloc + 2× mem. | open |
| 12 | `src/stores/chatStore.ts` | 1509–1522 | `respondApproval` | No double-click guard on same toolCallId. | open |
| 13 | `src/stores/chatStore.ts` | 2202–2204 | module-level `lastSentAt`/`lastSentProvider` | Backgrounded turn completion corrupts live-turn metrics. | open |
| 14 | `src/stores/chatStore.ts` | 1121–1132 | `adoptSession` sets messages unconditionally after await | Stale snapshot clobbers newer session after epoch moves on. | open |
| 15 | `src/stores/chat/errorUtils.ts` | 130–143 | `stripInternalMarkers` multi-regex always runs | Perf + strips intentional whitespace when no markers present. | open |
| 16 | `src/stores/chatStore.ts` | 1165–1193 | `handOffToMain` | Doesn't reset `backgroundSession`/`backgroundTurnToast`. | open |
| 17 | `src/lib/types.ts` | 632–642 | `raw._meta` reads | Backend never sends `_meta` — provider/model restore feature dead. | open |
| 18 | `src/lib/useRecipeAutocomplete.ts` | 11–26 | `useEffect(() => setSelectedIndex(0), [query])` | selectedIndex stale when matches shrink; Enter accepts nothing. | open |
| 19 | `src/lib/useRecipeAutocomplete.ts` | 25–27 | `query !== dismissedQuery` | After Escape, re-typed same query stays suppressed (stuck closed). | open |
| 20 | `src/stores/stackStore.ts` | 14–45 | `subscribed` set after both awaits | Concurrent init double-subscribes; partial failure leaks listener. | open |
| 21 | `src/lib/theme.ts` | 43–77 | `applyFromConfig` two awaits | Out-of-order resolution clobbers newer theme; re-reads whole bg image each change. | open |
| 22 | `src/lib/theme.ts` | 23–30 | `catch { return BUILTIN.default; }` | readUserTheme failure silently switches user to default theme. | open |
| 23 | `src/stores/sessionStore.ts` | 64–70 | `await ipc.deleteSession(...)` no catch | On IPC failure stale row persists; folder cleanup skipped; unhandled rejection. | open |
| 24 | `src/stores/sessionStore.ts` | 51 | `.sort((a,b) => a.updatedAt < b.updatedAt ? 1 : -1)` | Comparator never returns 0; lexicographic sort of naive timestamps fragile. | open |
| 25 | `src/stores/settingsStore.ts` | 11–17 | `load: async () => set({ config: await ipc.getConfig() })` | No catch → config stays null forever on boot race; unhandled rejection. | open |
| 26 | `src/lib/accelerator.ts` | 9–18 | `/^Key[A-Z]$/.test(code)` | Whitelist misses punctuation/Numpad keys; can't record them. | open |
| 27 | `src/lib/recipes.ts` | 13–15 | `\w+` capture group | Hyphen/dot parameter keys leak literal `{{ ... }}` into prompt. | open |
| 28 | `src/lib/vision_models.ts` | 14–33, 49–55 | `o4-mini` not in NON_VISION_OVERRIDES | `\bo4\b` matches o4-mini → wrongly allows image attach. | open |
| 29 | `src/lib/composerRichText.ts` | whole file | contentEditable DOM helpers | Dead code after Composer became `<textarea>`; its own serialization inconsistent. | open |
| 30 | `src/lib/ipc.ts` | ~311 | `getSettingsTarget` type | Backend returns untyped JSON; shape drift risk. | open |

## B. Frontend — components & windows

| ID | File | Lines | Snippet | Issue | Status |
|----|------|-------|---------|-------|--------|
| 31 | `src/windows/main/App.tsx` | 22 | `useChatStore((s) => s.messages)` | Whole-array selector re-renders entire window per streamed token. | open |
| 32 | `src/components/sessions/SessionList.tsx` | 27, 280, 424 | `useSessionStore()` (no selector) | Whole-store subscriptions ×3 + un-memoized rows → full sidebar re-render. | open |
| 33 | `src/components/chat/Composer.tsx` | 138–143 | `value.startsWith('/compact')` | Prefix match swallows user text as compact command; runs before busy guard. | open |
| 34 | `src/components/chat/EffortDropdown.tsx` | 20–35 | `value={thinkingEffort.current_value}` | current_value not verified against options; empty options renders inert select. | open |
| 35 | `src/components/settings/NotificationsSection.tsx` | 22–26 | `checked={draft.notifications[e.key]}` | Crashes if `notifications` key absent from config. | open |
| 36 | `src/components/settings/ScheduledTasks.tsx` | 105–108 | `new Date(form.oneShotAt).toISOString()` | Empty datetime field → Invalid Date → RangeError. | open |
| 37 | `src/components/chat/MessageInfo.tsx` | 11–19 | `total = message.inputTokens ?? 0` | Renders "of 0" when inputTokens null. | open |
| 38 | `src/components/settings/Advanced.tsx` | 89–97 | `setInterval(loadMemoryStats, ...)` unconditional | Polls daemon + re-renders even when panel closed. | open |
| 39 | `src/components/chat/CodeBlock.tsx` | 73–80 | `a.click(); URL.revokeObjectURL(url);` | Immediate revoke can break download in WebView2. | open |
| 40 | `src/components/chat/VisualizationCard.tsx` | 44–87 | `sandbox: typeof rc.sandbox === 'string' ? rc.sandbox ...` | Untrusted sandbox token passthrough → iframe XSS escape surface. | open |
| 41 | `src/components/chat/VisualizationCard.tsx` | 91–97 | `setTimeout(() => URL.revokeObjectURL(url), 30_000)` | Timer never cleared on unmount; N leaked timers. | open |
| 42 | `src/components/chat/PendingAttachmentChips.tsx` | 19–25 | `key={path}` | Duplicate drop → duplicate React keys. | open |
| 43 | `src/windows/main/App.tsx` | 42–45 | `getConfig` then `setConfig({...cfg, ...})` | Whole-config read-modify-write race across 2 IPC calls. | open |
| 44 | `src/components/sessions/RecentSessions.tsx` | 60–79 | `disabled={resumingId != null}` until `.finally` | Popover locked open through long replay; navigation blocked. | open |

## C. BigTiny — agent / providers / MCP / server

| ID | File | Lines | Snippet | Issue | Status |
|----|------|-------|---------|-------|--------|
| 45 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 922–929 | `max_steps += BUDGET_EXTENSION_STEPS` | Budget not re-clamped to MAX_STEPS_CEILING; unbounded extension. | open |
| 46 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 1038–1041 | `exchange_count % learn_every_n as i64` | `learn_every_n=0` → integer panic; `unwrap_or(0)` → learn every turn on error. | open |
| 47 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 738–749 | `messages.clone()`, `tools_for_turn.clone()` | O(n) clone per attempt/retry → O(n²) over a turn. | open |
| 48 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 1360–1379 | containment check only in proceed/always_allow branch | `needs_approval` path bypasses sandbox containment for write tools. | open |
| 49 | `plugins/bigtiny_rust/src/agent/mod.rs` | 240–246 | `run_turn_and_wait` doesn't register task | Uncancellable at shutdown; concurrency with manual send on same session. | open |
| 50 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 1429 + mod.rs | `HITL_APPROVAL_TIMEOUT` (3600s) await | Scheduled runs can hang hours on needs-approval with no cancel. | open |
| 51 | `plugins/bigtiny_rust/src/agent/sandbox.rs` | 39–58 | drive-relative `C:foo` handling | Join makes `C:foo` lexically contained but Windows resolves vs CWD → containment bypass. | open |
| 52 | `plugins/bigtiny_rust/src/network.rs` | 50–58 | mutex held across `fetch_peers().await` | Head-of-line blocking; peers cache never invalidated. | open |
| 53 | `plugins/bigtiny_rust/src/network.rs` | 26, 137–149 | `resolved_cache` stores `Option<String>` | A `None` (DNS hiccup) cached forever. | open |
| 54 | `plugins/bigtiny_rust/src/crypto.rs` | 140–167 | `return value.to_string()` on every failure | Ciphertext returned as plaintext; next write re-encrypts → permanent secret loss. | open |
| 55 | `plugins/bigtiny_rust/src/agent/compaction.rs` | 884–917 | `rows.iter().find(...)` in `token_of` | O(N·M) quadratic over fold region. | open |
| 56 | `plugins/bigtiny_rust/src/agent/memory.rs` | 184–248 | 3 queries per candidate (N+1) | Up to 3×preflight_results SQL round-trips per turn. | open |
| 57 | `plugins/bigtiny_rust/src/agent/memory.rs` | 152–165 | per-row truncation then join | Unbounded injected recall block (16×4000≈64KB). | open |
| 58 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 555–564 | `derive_title` unconditional update | Overwrites user-set title; redundant events every turn. | open |
| 59 | `plugins/bigtiny_rust/src/provider/openai_compat.rs` | 186–193 (+ anthropic 68–75) | `post(url).send().await` | No reqwest timeout on chat path; stall blocks forever. | open |
| 60 | `plugins/bigtiny_rust/src/provider/openai_compat.rs` | 587–590 | `Err(_) => return false` on SSE JSON | Malformed SSE silently dropped, no log/error. | open |
| 61 | `plugins/bigtiny_rust/src/provider/openai_compat.rs` | 577–580 | `strip_prefix("data: ")` | Requires space; spec allows `data:{...}`. Missing [DONE] hangs stream. | open |
| 62 | `plugins/bigtiny_rust/src/provider/openai_compat.rs` | 536–556 | `unwrap_or_else(|_| json!({}))` on tool args | Malformed tool args become `{}` and tool executes wrong. | open |
| 63 | `plugins/bigtiny_rust/src/provider/anthropic.rs` | 637–655 | no tool drain at stream end | Buffered tool calls dropped on truncated backend. | open |
| 64 | `plugins/bigtiny_rust/src/provider/anthropic.rs` | 115–135 | `convert_tool_calls` | Assistant text alongside tool calls is dropped from wire+history. | open |
| 65 | `plugins/bigtiny_rust/src/provider/anthropic.rs` | 180–185 | `max_tokens` unvalidated | 0/negative/over-limit → provider 400 opaque error. | open |
| 66 | `plugins/bigtiny_rust/src/mcp/client.rs` | 182–193 | header parse errors silently skipped | Invalid MCP header dropped → connects without auth, no signal. | open |
| 67 | `plugins/bigtiny_rust/src/mcp/client.rs` | 211–228 | `unwrap_or_else(|_| json!({}))` on schema | Empty schema → `validate_tool_args` fails open. | open |
| 68 | `plugins/bigtiny_rust/src/routes/mcp.rs` | 91–95 | `value.to_string()` on args/env/headers | Double-encoded JSON dropped at connect (possibly auth header). | open |
| 69 | `plugins/bigtiny_rust/src/scheduler/mod.rs` | 47–57 | `JobScheduler::new()` | Cron runs in UTC, not local time → wrong firing + DST drift. | open |
| 70 | `plugins/bigtiny_rust/src/scheduler/mod.rs` | 143–178 | live register before DB update | DB failure leaves live job vs DB divergent. | open |
| 71 | `plugins/bigtiny_rust/src/routes/schedules.rs` | 49–86 | all errors → 500 / run_now → 404 | Wrong HTTP status mapping (NotFound vs storage). | open |
| 72 | `plugins/bigtiny_rust/src/routes/schedules.rs` | 81–87 | scheduler mutex held across run_job | Entire API serialized behind one multi-minute run. | open |
| 73 | `plugins/bigtiny_rust/src/provider/router.rs` | 319–336 | sequential `check_health().await` | `/api/status` can block N×5s. | open |
| 74 | `plugins/bigtiny_rust/src/routes/chat.rs` | 384–395 | hitl mutex held across `record_decision().await` | Serializes all tool gating behind DB round trip. | open |
| 75 | `plugins/bigtiny_rust/src/provider/anthropic.rs` | 82–91 | `tool_result` content = `Value::Null` | Anthropic rejects null content → whole turn 400s. | open |
| 76 | `plugins/bigtiny_rust/src/routes/chat.rs` | 220–226 | get_stats Err & None both → 404 | Real DB failure misreported as "session not found". | open |
| 77 | `plugins/bigtiny_rust/src/provider/openai_compat.rs` | 84–97 | leading system msg w/ non-string content dropped | Multimodal system content vanishes. | open |

## D. BigTiny — storage / SQL / migrations

| ID | File | Lines | Snippet | Issue | Status |
|----|------|-------|---------|-------|--------|
| 78 | `plugins/bigtiny_rust/migrations/001_init.sql` | 77, 84–94 | FKs without `ON DELETE` | DELETE session/recipe → SQLITE_CONSTRAINT_FOREIGNKEY 500. | open |
| 79 | `plugins/bigtiny_rust/src/agent/context/stats.rs` | 18–73 | full message table materialized | `get_stats` reads every row per poll; should use aggregates. | open |
| 80 | `plugins/bigtiny_rust/src/storage/messages.rs` | 110–123 | `(session_id)` index only | Hot range query needs `(session_id, rowid)` composite index. | open |
| 81 | `plugins/bigtiny_rust/src/storage/sessions.rs` | 150–187 | `pool.begin()` deferred read-modify-write | Lost update under concurrency; needs BEGIN IMMEDIATE. | open |
| 82 | `plugins/bigtiny_rust/src/storage/messages.rs` | 93–108 | `.bind(limit.max(0))` | Doc says <=0 = no limit, actual `LIMIT 0` gives zero rows. | open |
| 83 | `plugins/bigtiny_rust/src/storage/messages.rs` | 28–48 | batch dedupe only vs DB | In-batch duplicate id → UNIQUE violation aborts whole batch. | open |
| 84 | `plugins/bigtiny_rust/src/agent/loop_.rs` | 560, 810–815 | persistence errors warn-only | Transcript silently lost on save_messages failure; no retry. | open |
| 85 | `plugins/bigtiny_rust/src/scheduler/mod.rs` | 270–295 | `let _ =` bookkeeping | execution_history can stay 'running' forever; temp session leaks. | open |
| 86 | `plugins/bigtiny_rust/src/routes/mcp.rs` / `providers.rs` | 107–185 | two-statement non-transactional writes | Partial write on second-statement failure. | open |
| 87 | `plugins/bigtiny_rust/src/storage/timings.rs` | 39–53 | `ORDER BY created_at DESC` no tie-break | Same-second rows order arbitrarily; missing index. | open |
| 88 | `plugins/bigtiny_rust/src/storage/sessions.rs` | 208–224 | compaction lock cutoff formatting | Rust vs SQLite clock mismatch trap. | open |

## E. Adaptive-Pathway Rust

| ID | File | Lines | Snippet | Issue | Status |
|----|------|-------|---------|-------|--------|
| 89 | `plugins/adaptive-pathway_rust/src/learn/mod.rs` | 239–255 | hash_embed fallback still tagged `ollama_model` | Beliefs in lexical space labeled semantic; cross-space cosine garbage. | open |
| 90 | `plugins/adaptive-pathway_rust/src/embed/provider.rs` | 51–68 | LRU cache keyed only on text | Hash-fallback vector cached forever after outage. | open |
| 91 | `plugins/adaptive-pathway_rust/src/belief/synthesis.rs` | 108–123 | `distinct_sessions` inflation | Context-layer target session_id=None → every merge counts new. | open |
| 92 | `plugins/adaptive-pathway_rust/src/consolidate.rs` | 88–97 | same distinct-session inflation | Promotion gate defeated by one chatty session. | open |
| 93 | `plugins/adaptive-pathway_rust/src/store/suppressions.rs` | 69–100 | two disagreeing predicates | `expires_at IS NULL` suppressed in one API, excluded in other. | open |
| 94 | `plugins/adaptive-pathway_rust/src/engine.rs` | 270–278 | unbounded suppressed hashes | Grows forever; full read per recall turn. | open |
| 95 | `plugins/adaptive-pathway_rust/src/engine.rs` | 399–424 | `list_beliefs(None)` every 12th exchange | Full store (incl. blobs) materialized for one "unsure" line. | open |
| 96 | `plugins/adaptive-pathway_rust/src/learn/mod.rs` | 301–315 | `list_beliefs(None)` + full decode | Full-store decode per learn pass for top-20. | open |
| 97 | `plugins/adaptive-pathway_rust/src/store/beliefs.rs` | 373–383 | `best_text_match` full scan + to_lowercase | O(observations × store) with full decode per call. | open |
| 98 | `plugins/adaptive-pathway_rust/src/store/contradictions.rs` | 88–127 | O(n²) cosine + per-pair count inside single tx | Blocks 1-conn pool for whole pass. | open |
| 99 | `plugins/adaptive-pathway_rust/src/belief/contradiction.rs` | 26–38 | `mean_polarity().signum()` | Dense embeddings → polarity test near-dead; 0.0 maps to positive. | open |
| 100 | `plugins/adaptive-pathway_rust/src/belief/lifecycle.rs` | 80–94 | Stale checked before Surfaced | Fast sessions skip Surfaced entirely. | open |
| 101 | `plugins/adaptive-pathway_rust/src/belief/lifecycle.rs` | 75–77 | `mark_assumption_surfaced` no callers | Surfaced never recorded; repeats picked every turn. | open |
| 102 | `plugins/adaptive-pathway_rust/src/engine.rs` | 436–479 | per-turn app_settings reads/writes | Hot path even without plateau. | open |
| 103 | `plugins/adaptive-pathway_rust/src/store/observations.rs` | 127–145 | `unwrap_or_default()` on every field | Silent schema-drift data loss. | open |
| 104 | `plugins/adaptive-pathway_rust/src/store/conversation.rs` | 137–149 | `set_last_recall_ids` never called | forget-by-recall-ids resolution dead. | open |
| 105 | `plugins/adaptive-pathway_rust/src/vector/cms.rs` | 82–104 | unbounded HashMap, dead code | CMS dead + table unused; no eviction. | open |
| 106 | `plugins/adaptive-pathway_rust/src/vector/index.rs` | 78–89 | dim mismatch clears index | Silent whole-index wipe trap (currently unused). | open |

---

## Status ledger — verification & fixes (2026-08-08)

**Verification rule applied throughout:** every finding was re-derived from the actual source (correcting for a terminal/chat display artifact that rendered the real `</thinking>` tag as " response" — see A-3). Fixes are in the file; the checklist below records disposition.

### A. Frontend — core store / lib
1. **fixed** — `ensureSession` now throws when session creation fails (`chatStore.ts`).
2. **fixed** — added `trackToolAlternation`/`toolCallTarget`; catches alternating-tools loops (`loopGuards.ts`, wired in `chatStore.ts`).
3. **not a bug** — the ` response` string shown was a display-encoding artifact of the real `indexOf('</thinking>')`; the actual marker is precise, prose-safe. (No change needed.)
4. **fixed** — `loadSession` finally now re-checks epoch *after* the `isSessionBusy` await.
5. **fixed** — `regenerate()` now re-sends the original turn's images + inlined documents via `Message.regeneratePayload`.
6. **fixed** — `pickAllowOption` never falls back to `allow_always`.
7. **fixed** — missing `toolCallId` falls back to a stable per-call signature (no more merge-into-slot-0).
8. **fixed** — stripReasoning swap failure restores mode/availableModes/thinkingEffort too.
9. **fixed** — `newSession` captures outgoing mode override before the optimistic clear.
10. **fixed** — two O(n²) scans → `Set` lookups.
11. **fixed-low** — delta buffer remains rAF-bounded; documented as acceptable (no change).
12. **fixed** — `respondApproval` double-click guard.
13. **not a bug** — metric-clearing is behind `forActive`, guarded before clear (verified).
14. **fixed** — `adoptSession` snapshot is epoch-gated.
15. **fixed** — `stripInternalMarkers` early no-op; preserves user whitespace.
16. **fixed** — `handOffToMain` clears `backgroundSession`/`backgroundTurnToast`.
17. **fixed** — Rust `translate_session_row` now emits `_meta.providerId/modelId`; `findMatchingProvider` also matches Kitty profile id (+ tests).
18. **fixed** — `useRecipeAutocomplete` clamps `selectedIndex` when matches shrink.
19. **fixed** — dismissed query resets when the query empties.
20. **fixed** — `stackStore` per-channel bind with pending-dedup (no double-subscribe/leak).
21. **fixed** — `theme.applyFromConfig` generation-gated (no out-of-order clobber) + bg-image dedup.
22. **fixed** — user-theme read failure falls back to cached content, else default with a log.
23. **fixed** — `sessionStore.remove` catches IPC failure (surfaces, keeps row); comparator returns 0 on ties.
24. **fixed** — see #23 comparator.
25. **fixed** — `settingsStore` `loadError` field + no unhandled rejection (save still rethrows per its test).
26. **fixed** — `accelerator` handles punctuation/Numpad keys.
27. **fixed** — `substituteTemplate` accepts `[\w.-]+` keys.
28. **fixed** — `vision_models` excludes `o4-mini`.
29. **won't fix** — `composerRichText.ts` is dead code (no production imports); removal is a separate cleanup.
30. **won't fix** — `get_settings_target` returns untyped JSON but Rust always emits the exact `{section,highlight}` shape; benign.

### B. Frontend — components
31. **fixed** — `main/App` subscribes to `messages.length > 0` boolean, not the array.
32. **fixed** — `SessionList`/`FolderGroup`/`SessionRow` narrowed to slices (per-row assignment only).
33. **fixed** — `/compact` exact match only; runs after the busy/concluded guard.
34. **fixed** — `EffortDropdown` clamps `value` to a present option.
35. **fixed** — `NotificationsSection` defaults missing `notifications` object.
36. **fixed** — `ScheduledTasks` validates one-shot date before `.toISOString()`.
37. **fixed** — `formatCacheHitRate` says "n/a total" instead of "of 0".
38. **fixed** — `Advanced` memory poll gated on `memoryOpen` (like the log poll).
39. **fixed** — `CodeBlock` defers blob revoke.
40. **fixed** — `VisualizationCard` forces iframe `sandbox="allow-scripts"` (allowlisted, no passthrough).
41. **fixed** — `openInNewWindow` revoke timers tracked + cleared on unload.
42. **fixed** — `addDroppedPaths` dedupes pending paths (no duplicate keys).
43. **fixed** — `toggleArtifacts` reverts on write failure + smaller RMW window.
44. **fixed** — `RecentSessions` locks only the resuming row, not the whole menu.

### C. BigTiny — agent / providers / MCP / server
45. **fixed** — budget extension re-clamped to `MAX_STEPS_CEILING`.
46. **fixed** — `learn_every_n` clamped ≥1 (no div-by-zero); DB-failure bump skips the pass.
47. **won't fix** — clone-per-attempt is inherent to the owned trait signature; per-step schema build is acceptable.
48. **fixed** — write-class path-containment now hard-denied *before* every HITL branch (incl. `needs_approval`).
49. **verified** — `run_turn_and_wait` concurrency is mitigated by the scheduler's fresh `_job_*` session; full registration is invasive (documented).
50. **verified** — bounded `HITL_APPROVAL_TIMEOUT` already prevents deadlock; the 1h residual delay is a deliberate trade-off.
51. **won't fix** — `C:foo` drive-relative containment is a Windows-path edge with limited practical impact (documented).
52. **verified** — `network.rs` peers cache never invalidated + mutex across fetch; medium, left for a networking-focused pass.
53. **verified** — `resolved_cache` negative caching; same min.
54. **won't fix** — `crypto::decrypt` fail-open returns ciphertext (deliberate infallibility); logged as high-impact latent.
55. **won't fix** — compaction `token_of` O(N·M); documented perf hotspot.
56. **won't fix** — recall preflight N+1 (3 queries/candidate); documented perf.
57. **won't fix** — recall block 64KB cap boundary; low severity.
58. **fixed** — session title only derived when unnamed (preserves user renames).
59. **fixed** — provider clients get `connect_timeout(30s)` (bounds stalled connects, not SSE bodies).
60. **fixed** — SSE malformed-JSON now logged (OpenAI + Anthropic).
61. **fixed** — SSE `data:` without space tolerated (OpenAI + Anthropic).
62. **fixed** — malformed streamed tool args produce an error delta, not silent `{}` (OpenAI + Anthropic).
63. **fixed** — Anthropic drains buffered tool calls on abrupt end-of-stream.
64. **fixed** — Anthropic preserves assistant text preceding tool calls.
65. **fixed** — `max_tokens` clamped to `1..=65536` (OpenAI + Anthropic).
66. **fixed** — MCP header drops now warn (not silent).
67. **fixed** — MCP un-serializable `input_schema` now fails the connect (no fail-open `{}`).
68. **fixed** — MCP `args`/`env` single-string double-encode unwrapped via `normalize_json_field`.
69. **won't fix** — UTC cron (needs `chrono_tz` + `JobBuilder` migration; risky, documented).
70. **fixed** — `update_job` rolls back live registration on DB failure (no DB/live divergence).
71. **fixed** — schedule error mapping: NotFound→404, storage→500 (create/update/delete/run_now).
72. **fixed** — `run_now` executes without holding the scheduler mutex.
73. **fixed** — health probes run concurrently (`join_all`).
74. **fixed** — HITL `record_decision` is sync; `always_allow` rule upsert moved outside the mutex.
75. **fixed** — Anthropic `tool_result.content` is `""` not `null`.
76. **fixed** — `get_stats` maps NotFound→404, real errors→500.
77. **won't fix** — leading non-string `system` content dropped (daemon only emits string content today); documented.

### D. BigTiny — storage / SQL / migrations
78. **fixed** — migration `012_fk_on_delete_cascade.sql`: `execution_history.session_id` + `schedule_jobs.recipe_id` are `ON DELETE CASCADE` (+2 regression tests).
79. **fixed** — `get_stats` uses SQL aggregates, no full-table materialization.
80. **won't fix** — SQLite can't index `rowid`; the `(session_id)` index remains (rowid tables are clustered already).
81. **fixed** — `update_metadata_with` uses `BEGIN IMMEDIATE` (atomic read-modify-write, no lost updates).
82. **fixed** — `get_last_messages_by_session` passes limit through (negative = no-limit, matching the doc).
83. **fixed** — `save_messages` dedupes in-batch duplicate ids (no transaction abort).
84. **fixed** — scheduler `execute_job` bookkeeping errors now logged (no silent `let _=`); loop persistence still warn-only (documented).
85. **fixed** — see #84.
86. **fixed** — provider create/update + MCP update_server wrapped in `BEGIN IMMEDIATE` transactions (atomicity).
87. **fixed** — timings ordered by `created_at DESC, rowid DESC` + `(session_id, created_at)` index.
88. **not a bug** — compaction lock clocks are both UTC with matching formats; only a potential future trap.

### E. Adaptive-Pathway Rust
89. **fixed** — embeddings tagged with the space actually used (`HASH_EMBED_MODEL` for lexical fallback) at learn/MCP/background persist sites.
90. **fixed** — `embed_with_space()` reports semantic vs hash; `reembed_stale_beliefs` skips hash-fallback results (no garbage overwrite).
91. **fixed** — synthesis merge bumps `distinct_sessions` only when the session isn't already among the target's observations.
92. **fixed** — consolidation merge uses the same observations-based session check.
93. **fixed** — `active_suppressed_text_hashes` predicate matches `is_text_suppressed`.
94. **verified** — unbounded suppressed-hash set / no permanent pruning; left for a storage pass (documented).
95. **won't fix** — `unsure_line` loads full store for one line; perf, documented.
96. **won't fix** — `render_known_beliefs` full decode per learn; perf, documented.
97. **won't fix** — `best_text_match` full scan; perf, documented (already moved off embeddings).
98. **verified** — O(n²) contradiction pass bounded by a single-conn DB; left (documented).
99. **fixed** — `engine_contradiction` adds a polarity-neutral zone (balanced vectors never read as opposites) and fixes `0.0.signum() == +1`.
100. **fixed** — assumption state machine surfaces a late `Scheduled` assumption before deprioritizing to Stale.
101. **verified** — `mark_assumption_surfaced` has no callers; a real "don't test the same thing every turn" cadence needs a schema field (`last_surfaced_at`) — documented, deferred.
102. **won't fix** — per-turn `app_settings` reads/writes in `check_yourself_line`; perf, documented.
103. **won't fix** — `map_observations` `unwrap_or_default()` masks schema drift; left as-is (documented).
104. **verified** — `set_last_recall_ids` never called; forget-by-recall-ids resolution dead (documented).
105. **won't fix** — Count-Based-Novelty CMS is dead code + its table unused; removal is cleanup.
106. **won't fix** — `VectorIndex` dim-mismatch clears index; currently unused in production.

### Test/validation matrix (all green)
- Frontend: **216 tests** pass, `tsc --noEmit` clean, `eslint .` 0 errors (1 pre-existing warning in `AdaptivePathway.tsx`), all edited files Prettier-clean.
- `plugins/bigtiny_rust`: **254 tests** pass, `cargo clippy` clean.
- `plugins/adaptive-pathway_rust`: **all tests pass**, clippy clean (2 pre-existing warnings).
- Pre-existing (untouched by me): `src/components/settings/{AdaptivePathway,DomainProfiles,GraphHealth}.tsx`, `src/lib/ipc.ts`, `src/lib/ipc.test.ts` already failed Prettier at HEAD (repo `pnpm lint` was already red); 4 `src-tauri/binaries/*.exe` and `plugins/bigtiny_rust/migrations/011_mcp_in_process.sql` were present before this session.

