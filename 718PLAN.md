# 718PLAN.md — Kitty Bug Fixes & Behavior Changes

## Issue 1: Delete Chat → New Chat Shows "Internal Error"

### Root Cause
goosed returns `"Internal error"` (the JSON-RPC catch-all) on the first prompt of a freshly created session when the prior session was deleted moments before. Contributing factors:

1. **`newSession()` has no try/catch** (`chatStore.ts:784-838`) — if `getOrCreateSession(cwd)` throws, the error propagates uncaught (the `void` on the button swallows it). State stays at `sessionId: null, creatingSession: true` — broken UX.

2. **`computercontroller` seed task races with first prompt** (`crud.rs:76-86`) — a fire-and-forget task sends `_goose/unstable/session/extensions/add` via the same ACP WebSocket. If the user sends a prompt before this completes, goosed may internally clash and return "Internal error."

3. **`chat://error` handler clears `acp = None`** (`prompt.rs:115`) — after an "Internal error" on the old session's in-flight turn, the shared ACP client reference is dropped. The next call reconnects, but the transient disconnection adds fragility.

4. **`forActive` guard is robust** (`chatStore.ts:1503`) — prevents stale events from leaking across sessions (when `session://deleted` arrives before `chat://error`). This is NOT the source of the bug; it's what saves us from it being worse.

### Fix

**A. Add try/catch to `newSession()`** (`src/stores/chatStore.ts`, lines 827-836):
- Wrap `getOrCreateSession(cwd)` in try/catch.
- On failure, reset `creatingSession: false` and set `error` so the user sees a message.

**B. Await the `computercontroller` seed task before returning from `new_session`** (`src-tauri/src/commands/session/crud.rs`, lines 74-86):
- Remove `tauri::async_runtime::spawn`. Directly `.await` the extension-add call (with a `.ok()` to ignore failure). This was already documented as a performance optimization (Round-7) but introduces a race. The ~5-20ms ACP round trip is negligible compared to model load time. Update the comment.

**C. Add a `newSession()` error guard in `send()`** (`src/stores/chatStore.ts`, lines 1232-1234):
- Move `ensureSession()` inside the try/catch block (currently only `sendPrompt` is inside it). If session creation fails, `error` is set and `busy: false` is restored.

**D. (Optional defense-in-depth) Clear `in_flight_sessions` entry when `delete_session` succeeds** (`crud.rs`, line 277-279):
- After `session/delete` succeeds, also remove the session from `in_flight_sessions`. This ensures any background `send_prompt` task for the deleted session doesn't later emit events with that session's ID (though `forActive` already guards against this).

### Files Changed
| File | Change |
|------|--------|
| `src/stores/chatStore.ts` | Wrap `getOrCreateSession()` in try/catch; move `ensureSession()` inside send()'s try/catch |
| `src-tauri/src/commands/session/crud.rs` | Await computercontroller seed task before returning from new_session |

---

## Issue 2: Slow Chat Initialization Before First Message

### Current Behavior
On a fresh app start, the first user message can take a long time before any response appears. The user sees no progress indicator during this wait.

### Analysis — What Actually Causes the Delay

The startup chain in `lifecycle/mod.rs::start_stack` (spawned in the background at app launch):

```
Step 1: Ensure Ollama running          (~1-5s, only if local provider or AP enabled)
Step 2: Spawn `goose serve`            (~1-10s, polls TCP port every 250ms × 40)
Step 2b: If Ollama provider: warm model (~10-60s, `POST /api/generate` with empty prompt)
Step 2c: Start AP sidecar (if enabled)
Step 3: Health loop
```

**Ollama model loading is already correctly conditional.** `active_ollama_target()` (`config/providers/mod.rs:92-102`) returns `None` for non-Ollama providers (Anthropic, OpenAI, OpenRouter, etc.), so `keep_alive_load` is never called for remote providers. It also only fires at startup, never per-session. **No code change is needed for this conditional.**

The actual delays, broken down by provider type:

| Delay Source | Local Ollama | Remote Provider |
|---|---|---|
| Ollama ensure_running | 1-5s | 0s (skipped if not `stack_needs_ollama`) |
| goosed spawn (port poll) | 1-10s | 1-10s |
| Model warmup (`keep_alive_load`) | 10-60s | 0s (skipped) |
| First ACP `ensure_client()` (WS connect + `initialize`) | ~10-50ms | ~10-50ms |
| First `session/new` ACP call | ~5-20ms | ~5-20ms |
| `ensureSafeApprovalMode()` ACP call | ~5-20ms | ~5-20ms |
| **Real inference delay** | Model load if not yet warm | Remote provider cold-start/routing |
| **Total ACP overhead** | ~20-100ms | ~20-100ms |

Key observations:
- **ACP round trips are negligible** (<100ms total) — not the bottleneck.
- **For remote providers**, the delay is mostly goosed spawn (1-10s) plus whatever the remote provider's first cold call costs — outside Kitty's control. The goosed.rs comment at line 68-73 confirms: *"a live provider switch consistently measured ~500-540ms total for kill+respawn+readiness ... The reported slowness is the first real inference call to the newly-active provider (e.g. OpenRouter routing/model cold-start), which happens inside goosed's own outbound request and is outside Kitty's control to fix."*
- **For local Ollama**, model loading (10-60s) dominates. The current `keep_alive_load` at startup helps — but only if the user waits for `start_stack` to finish. If they send immediately, the inference itself triggers the load.
- **No delay sources fire per-session.** Ollama warmup, goosed spawn, and AP initialization all happen once at startup.

### Fix

**A. Parallelize goosed spawn and model warmup** (`src-tauri/src/lifecycle/mod.rs`, steps 2 and 2b):
- Both are independent: goosed doesn't need a warm model to accept sessions; the warmup request goes to Ollama, not goosed.
- Replace sequential `.await` with `tokio::join!` so they overlap:
  ```rust
  // Before: sequential (total = spawn_time + warmup_time)
  goosed::spawn(env, override).await;
  if let Some((base, model)) = warm {
      keep_alive_load(&base, &model).await;
  }
  
  // After: parallel (total = max(spawn_time, warmup_time))
  let (goose_result, _) = tokio::join!(
      goosed::spawn(env, override),
      async {
          if let Some((base, model)) = warm {
              keep_alive_load(&base, &model).await;
          }
      }
  );
  ```
- This cuts wall-clock startup time for local-Ollama users from `spawn+warmup` to `max(spawn, warmup)` — a ~10-60s reduction.

**B. Remove unnecessary `ensureSafeApprovalMode()` from `newSession()`** (`src/stores/chatStore.ts`, line 837):
- The mode comes back correctly from goosed's `session/new` response (`SessionInfo.current_mode`).
- If chat mode needs `approve`, only force it when `isChatMode()` is true AND `current_mode !== 'approve'` — otherwise skip the extra ACP round trip.
- This saves one serial ACP call (~5-20ms, negligible but wasteful on principal).

**C. Show a startup-progress indicator in the overlay/main windows:**
- Add a `starting` phase to `StackStatus` that tracks: `spawning_goosed`, `warming_model`, `ready`.
- Frontend: when `status === 'starting'` and `warming_model === true`, show "Warming model…" in the composer area with a progress indicator, and disable send until the model is warm (for local providers) or goosed is ready (for remote providers).
- This doesn't reduce the delay but eliminates the "is it broken?" perception — the user sees what's happening.

**D. (Optional) Consider `keep_alive_load` approach for local Ollama:**
- The current `keep_alive_load` sends `POST /api/generate` with `{"prompt": "", "stream": false}` — this blocks until the model is fully loaded into GPU/RAM.
- Whether this is worth doing at all depends on user behavior: if the user typically waits before sending, the warmup pays off. If they send immediately, Ollama loads the model anyway on the first real inference — the warmup is redundant.
- No change needed here; just documented for awareness.

### Files Changed
| File | Change |
|------|--------|
| `src-tauri/src/lifecycle/mod.rs` | Parallelize goosed spawn + model warmup via `tokio::join!` |
| `src/stores/chatStore.ts` | Conditionalize `ensureSafeApprovalMode()` in `newSession()` — skip if mode is already `approve` |
| `src-tauri/src/lifecycle/health.rs` | Add `warming_model` / `spawning_goosed` phases to `StackStatus` |
| `src/components/chat/Composer.tsx` (or overlay App) | Show startup progress indicator based on `StackStatus` phases

---

## Issue 3: Permission Model — Files Outside Chat Folder & Security-Sensitive Shell Commands

### Current Behavior
- **Chat mode**: `decideChatApproval()` in `approvalUtils.ts` auto-rejects file ops outside the session's `cwd` (except goose cache). Shell commands with no structured path are always auto-approved.
- **Agentic mode**: No Kitty-side restrictions at all — approval mode is whatever the user set in goosed.

### Target Behavior
- **Files outside `cwd` + goose cache**: Show an approval prompt so the user can choose, rather than auto-rejecting.
- **Security-sensitive shell commands** (ssh, sudo, curl -o, rm -rf, etc.): Show an approval prompt regardless of mode.

### Fix

**A. Refactor `decideChatApproval()` to return a tri-state** (`src/stores/chat/approvalUtils.ts`, lines 63-86):
```typescript
type ApprovalDecision = 'allow' | 'reject' | 'prompt';

export function decideChatApproval(
  rawInput: unknown,
  cwd: string | null,
  options: { optionId: string }[]
): { decision: ApprovalDecision; optionId: string | null; warning?: string }
```
- File inside `cwd` → `'allow'` (unchanged)
- Goose internal cache → `'allow'` (unchanged)
- File outside `cwd`, not cache → change from `'reject'` to `'prompt'`
- Shell command — security-sensitive → `'prompt'`
- Shell command — not security-sensitive → `'allow'`
- No path, no command → `'allow'`

**B. Add `isSecuritySensitiveCommand()`** (`src/stores/chat/approvalUtils.ts`):
- Regex patterns for: `ssh`, `scp`, `sftp`, `sudo`, `su`, `rm -rf`, `chmod`, `chown`, `curl` with `-o`, `wget` with `-O`, `netsh`, `iptables`, `shutdown`, `format`, `diskpart`, `nc`/`ncat`, `telnet`, `taskkill`, etc.
- Returns `true` if the shell command matches any pattern.

**C. Update `onApprovalNeeded` handler in chatStore** (`src/stores/chatStore.ts`, lines 1717-1764):
- When `decision === 'prompt'`, queue the event into `pendingApprovals[]` instead of auto-responding.
- When `decision === 'allow'`, auto-approve as before.
- When `decision === 'reject'`, auto-reject as before.

**D. Ensure `ApprovalPrompt` renders in chat mode** (find the component that hides it):
- In chat mode, `ApprovalPrompt` is currently filtered out. When `pendingApprovals` has items, it should render. Check `ChatView.tsx` or the `chatMode` guard around the approval prompt render.

**E. Apply security-sensitive-command check in ALL modes** (not just chat mode):
- The `isSecuritySensitiveCommand()` check should run in both chat and agentic modes. If a security-sensitive shell command is attempted in `auto` approval mode, force a prompt anyway — this is a safety net.

### Files Changed
| File | Change |
|------|--------|
| `src/stores/chat/approvalUtils.ts` | Add `ApprovalDecision` type, tri-state return, `isSecuritySensitiveCommand()` |
| `src/stores/chatStore.ts` | Handle `prompt` decision in `onApprovalNeeded`; apply shell check in all modes |
| `src/components/chat/ChatView.tsx` (or equivalent) | Stop hiding ApprovalPrompt in chat mode when pendingApprovals populated |
| `src/stores/chatStore.pathwithin.test.ts` | Add tests for `prompt` decision and `isSecuritySensitiveCommand()` |
| `docs/VERSIONS.md` | Update chat-mode description to reflect tri-state model |

---

## Issue 4: Thought Container Open by Default → Closed by Default

### Current Behavior
In `ThinkingBox.tsx:29`: `const open = pinned ?? streamingReasoning;` where `streamingReasoning = streaming && !hasAnswer`. This auto-expands the thinking box while reasoning is actively streaming and no answer text has arrived yet. When answer text starts, it auto-collapses.

### Target Behavior
Thought container is closed by default. User must click to expand it.

### Fix

**One-line change** (`src/components/chat/ThinkingBox.tsx`, line 29):
```diff
- const open = pinned ?? streamingReasoning;
+ const open = pinned ?? false;
```
Keep the `streamingReasoning` variable for its secondary use on line 35 (the label: "Thinking…" while streaming vs. "Thinking" when done).

Additionally, update the JSDoc comment (lines 5-12) to reflect the new behavior.

### Files Changed
| File | Change |
|------|--------|
| `src/components/chat/ThinkingBox.tsx` | Change default visibility to `pinned ?? false`; update JSDoc |

---

## Issue 5: Artifacts Pane Should Reflect Kitty/chats Folder Contents

### Current Behavior
Artifacts are derived exclusively from tool-call metadata (`deriveArtifact()` in `messageUtils.ts:80-99`). The pane uses a 5-second polling loop to prune artifacts whose files no longer exist on disk, but it never discovers NEW files that were created without a qualifying tool-call event (e.g., shell-produced files, files created outside the current session, tool calls whose names don't match the write-verb regex).

### Target Behavior
The artifacts pane should show files actually present in the session's `cwd` (the `Documents/Kitty/chats/<id>/` folder).

### Fix

**A. Add a filesystem-based artifact refresh** (`src/components/artifacts/ArtifactsPane.tsx`):
- On mount and on session change (watch `cwd`), call a new `listDirectory(cwd)` command to get actual files.
- Merge filesystem-derived artifacts with tool-call-derived ones, deduplicating by path.
- Keep the existing 5-second pruning loop but also ADD artifact entries for any files in the directory that aren't already in the list.

**B. Add `list_directory` Rust command** (`src-tauri/src/commands/file.rs`):
- Lists files (not directories) in a given path, returns `{ name, path, size, modified }`.
- Respect a reasonable max-file count and skip hidden files/directories.

**C. Distinguish source in the UI**:
- Tool-call-derived artifacts keep their tool name badge.
- Filesystem-derived artifacts show "disk" or no tool tag.
- Combine both lists, sorting by modification time.

### Files Changed
| File | Change |
|------|--------|
| `src-tauri/src/commands/file.rs` | Add `list_directory` command |
| `src-tauri/src/commands/mod.rs` | Register new command |
| `src/lib/ipc.ts` | Add `listDirectory()` wrapper |
| `src/components/artifacts/ArtifactsPane.tsx` | Add filesystem scan on mount + session change; merge with tool-derived artifacts |
| `src/stores/chatStore.ts` | Add `refreshArtifactsFromDisk()` action that calls `listDirectory` and merges |

---

## Issue 6: Graph Health Preferences Pane Mismatch

### Root Cause
The `GraphHealth.tsx` component calls `GET /health` and `GET /state` — both read live, in-memory data from the sidecar process. The data IS live (not cached). However:

1. **Only a subset of available data is shown.** The `health.py` `get_graph_health()` method returns a rich `GraphHealth` object (total edges, confidence %, tier distribution, hotspot details, override rate) — but this is NOT exposed via any HTTP endpoint the UI calls. The `/health` endpoint only returns a flat issue list (`ap.health_check()`).

2. **The sidecar must be running.** If the Adaptive Pathway sidecar is `down`, all calls short-circuit with `require_ok()` and the pane shows "Loading…" indefinitely or shows empty state.

3. **Maintenance may be stale.** The maintenance loop (confidence decay, cold-edge pruning) runs every 24 hours by default. In-memory confidence values can therefore drift from what's reasonable given the raw annotation data in the SQLite database.

### Fix

**A. Expose `get_graph_health()` via the sidecar HTTP API** (`plugins/adaptive-pathway/src/adaptive_pathway/integrations/sidecar/server.py`):
- Add a new `GET /graph_health` endpoint that calls `ap.health_checker.get_graph_health()` and returns the full `GraphHealth` object.
- This exposes: `total_edges`, `high_confidence_pct`, `flagged_hotspots`, `last_override_rate`, `blocking_issues`, `dimensionality_health`, `ensemble_health`, `novelty_health`, `tier_distribution`, `hotspot_details`.

**B. Add Rust client function and Tauri command** (`src-tauri/src/adaptive_pathway/mod.rs`, `src-tauri/src/commands/adaptive_pathway.rs`):
- Add `get_graph_health(base)` function calling `GET /graph_health`.
- Add `graph_health` Tauri command returning the structured data.
- Add `GraphHealth` type to frontend types.

**C. Update `GraphHealth.tsx` to show richer data** (`src/components/settings/GraphHealth.tsx`):
- Call the new `graph_health` endpoint.
- Show: total edges, confidence distribution, tier counts (hot/warm/cold), hotspot list with edge IDs, override rate.
- Keep existing `/state` and `/health` data as well for the live metrics.

**D. Add a "Status" indicator at the top** showing whether the sidecar is running, and a clear "Sidecar not running — enable Adaptive Pathway first" message when it's down.

### Files Changed
| File | Change |
|------|--------|
| `plugins/adaptive-pathway/src/adaptive_pathway/integrations/sidecar/server.py` | Add `GET /graph_health` endpoint |
| `src-tauri/src/adaptive_pathway/mod.rs` | Add `get_graph_health()` HTTP client function |
| `src-tauri/src/commands/adaptive_pathway.rs` | Add `graph_health` Tauri command |
| `src/lib/ipc.ts` | Add `adaptivePathwayGraphHealth()` wrapper |
| `src/lib/types.ts` | Add `GraphHealth` type |
| `src/components/settings/GraphHealth.tsx` | Fetch & display full graph health data; add sidecar-status indicator |

---

## Issue 7: replacement-mcp — File Editor Truncation & Workspace max_depth

### 7a. "Do not truncate the file editor"

**Current behavior** (`plugins/replacement-mcp/lean_mcp.py`, lines 178-187):
On `write`, the full content IS written to disk, but the echo back to the LLM is gated: if the content exceeds 500 words (`file_echo_max_words`), a JSON summary is returned instead of the actual content. The LLM cannot verify what it just wrote.

**Fix** (`lean_mcp.py`, lines 178-187):
Remove the echo suppression gate. Always return content with the write response.
```diff
-    if wc <= THRESH.get("file_echo_max_words", 500):
-        return f"{PREFIX_OK}Wrote {wc} words to {resolved}:\n{content}"
-    payload = json.dumps({...})
-    return f"{PREFIX_OK}{payload}"
+    return f"{PREFIX_OK}Wrote {wc} words to {resolved}:\n{content}"
```
Also remove the now-unused `import json` if no other usage remains (check first).

Optionally remove or comment the `file_echo_max_words: 500` threshold in `tool_prompts.yaml:25`.

### 7b. "max_depth for analyze workspace should be 10"

**Current behavior**: Already set to 10.
- `tool_prompts.yaml:26`: `workspace_max_depth: 10`
- `lean_mcp.py:290`: `THRESH.get("workspace_max_depth", 10)`

**No changes needed.** This is already correct.

### Files Changed
| File | Change |
|------|--------|
| `plugins/replacement-mcp/lean_mcp.py` | Remove echo-suppression gate on write (lines 178-187) |
| `plugins/replacement-mcp/tool_prompts.yaml` | Remove or comment `file_echo_max_words: 500` |

---

## Implementation Order (Recommended)

1. **Issue 7 (replacement-mcp)** — simplest, 5-minute change, no cross-dependency
2. **Issue 4 (ThinkingBox default)** — one-line CSS/state change, no cross-dependency
3. **Issue 2 (slow initialization)** — mostly Rust-side reordering, no UI restructuring
4. **Issue 5 (Artifacts pane)** — new Rust command + frontend merge logic
5. **Issue 1 (delete→error)** — try/catch additions + seed task await
6. **Issue 3 (permissions)** — most complex, touches approval decision engine + UI
7. **Issue 6 (Graph Health)** — new sidecar endpoint + Rust command + frontend

Issues 1-3 share `chatStore.ts` — do them sequentially or handle merge conflicts carefully.

## Verification Checklist Per Issue

### Issue 1
- [ ] Delete a session, immediately click "New Chat", send a message — no "Internal Error"
- [ ] `newSession()` failure (simulate by stopping goosed) shows an error message, not silent blank state
- [ ] `cargo clippy` + `pnpm lint` clean

### Issue 2
- [ ] Cold start with local Ollama provider: startup progress indicator shows "Warming model…" and send is gated until ready
- [ ] Cold start with remote provider (Anthropic/OpenAI): startup indicator shows goosed-spawning phase, no model warmup phase
- [ ] `keep_alive_load` is NOT called for remote providers (verify in logs/tracing)
- [ ] `ensureSafeApprovalMode()` is skipped when `current_mode` from `session/new` is already `approve`
- [ ] No regression: subsequent messages are as fast as before

### Issue 3
- [ ] Chat mode: file access outside chat folder shows approval prompt (not auto-reject)
- [ ] Chat mode: `ssh user@host` shows approval prompt
- [ ] Chat mode: safe shell commands (`python script.py`) auto-approve
- [ ] Agentic mode: security-sensitive commands still prompt even in `auto` mode
- [ ] Goose internal cache paths auto-approve (no regression)

### Issue 4
- [ ] Reasoning panel is collapsed when streaming starts
- [ ] User can click to expand it; expanded state persists
- [ ] "Thinking…" label still shows with ellipsis during active reasoning

### Issue 5
- [ ] Dropping a file into the chat folder (via Explorer) makes it appear in artifacts pane within the polling interval
- [ ] Shell-produced files appear in artifacts pane
- [ ] Tool-call-derived artifacts still work (no regression)
- [ ] Duplicate paths are deduplicated

### Issue 6
- [ ] Graph Health shows total edge count, tier distribution, hotspots
- [ ] Clear message when sidecar is not running
- [ ] Refresh button updates data live
- [ ] No regression: existing /state and /health data still shown

### Issue 7a
- [ ] Writing a file with >500 words echoes the full content back
- [ ] Writing a file with <500 words still works (no regression)

### Issue 7b
- [ ] Verify `max_depth=10` is the active value (already correct, just confirm)
