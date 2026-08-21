# BigTiny Rust Daemon — Stability Improvement Project

Goal: make `plugins/bigtiny_rust` the rock-solid core of the app on both Windows and
Android — absolutely stable, and graceful in the face of unstable connections to LLM
providers and MCP tool servers.

All paths are relative to `plugins/bigtiny_rust/`. Findings were produced by a full
read of the crate plus deep-dives into the vendored `rmcp` 0.9.1 and `sqlx` 0.8.6
sources (much of the transport risk lives in those dependencies).

---

## Order of Operations

Landed tier-by-tier with a checkpoint at each boundary: **`cargo test` + `cargo clippy`
must be green** in `plugins/bigtiny_rust/` before starting the next tier. The
"Already solid" list below is treated as regression constraints — never change those
behaviors. Android gating check per tier: `cargo ndk -t arm64-v8a --platform 26
check --lib` in `src-tauri/`.

1. **Tier 0 — Upgrade safety (one commit).** #30 legacy-bootstrap migration inserts
   wrapped in `BEGIN IMMEDIATE ... COMMIT` — prevents a bricked upgrade. **DONE (2026-08-20).**
2. **Tier 1 — Data loss / hangs / unbounded retry**, in order #1 → #2 → #3 → #4 → #5.
    Mid-stream provider errors (incl. the swallowed SSE `error` events) route through
    the shared retry/failover budget; frontend-visible `error_type` wire tag requires
    `pnpm test` / `pnpm lint` at root before closing the tier. **DONE (2026-08-20).**
3. **Tier 2 — Provider health & memory safety**, in order #6 → #7 → #8 → #9 → #10 →
    #11 → #12 → #13 (passive circuit breaker, buffer caps, DNS/cache TTLs, keepalive,
    transport taxonomy, no silent client degradation, summarizer ceilings). **DONE (2026-08-20).**
4. **Tier 3 — MCP transport robustness.** **Decision gate first:** investigate rmcp
   2.x — does 2.2.0 (already pinned by `kitty-tools`) fix the stdio codec
   (`new_with_max_length` default), decode-error resume, write-side send wedge, and
   cancellation? If viable, migrate `src/mcp/` to 2.x and unify the two rmcp pins; if
   not, fall back to a targeted `[patch.crates-io]` fork of 0.9.1. Either way #14/#15
   (max frame length + skip-and-continue) and #16 (write-side timeout/eviction) and
   #17 (`notifications/cancelled`) ride on it. Then #18 → #19 → #20 → #21 → #22 → #23.
5. **Tier 4 — Turn lifecycle / HITL / scheduler / SSE**, in order #24 → #25 → #26 →
   #27 → #28 → #29 → #31. #27 (nullable `execution_history.session_id`) and #28
   (retention/pruning) are sqlx **migrations** with a conservative policy: prune
   `execution_history`/`llm_timings` older than 30 days, cap messages per session,
   `wal_checkpoint(TRUNCATE)` at boot + daily. Final checkpoint: full test/clippy +
   Android gating check.

---

## Tier 1 — Data loss / hangs / unbounded retry (do first)

| # | Description | Where to change |
|---|-------------|-----------------|
| 1 | **Mid-stream provider failures bypass retry/failover entirely and retry unboundedly.** A connection drop or idle timeout arrives as a `Delta{finish_reason:"error", error_type:"request"}` (`openai_compat.rs:902-930`, `anthropic.rs:842-866`), but `process_stream` never reads `delta.error_type` (`loop_.rs:1640-1755`), so the turn sees a non-`stop` finish_reason and does `step+=1; continue` (`loop_.rs:1370-1381`) — re-calling the same provider with **no backoff, no failover, no shared retry budget**, up to `max_steps` times per turn. The retry/backoff/failover block (`loop_.rs:1090-1200`) only wraps pre-stream `Err`s. | `src/agent/loop_.rs:1640-1755` (read `error_type`, return a `ProviderError::Request` from `process_stream`), `loop_.rs:1090-1200` (route stream errors through the same retry/failover path), `loop_.rs:1370-1381`. |
| 2 | **Non-2xx response body is read with no timeout — a stalled error body hangs the turn forever.** After headers arrive, `resp.text().await` (`openai_compat.rs:455`, `anthropic.rs:447`) has no bound and the shared client only sets `connect_timeout`; the SSE idle timeout only wraps the *success* stream. A captive portal/proxy that returns `500`/`429` headers then stalls the body blocks indefinitely. It also materializes the whole body though only ~300 chars are surfaced. | `src/provider/openai_compat.rs:455`, `src/provider/anthropic.rs:447` — wrap in `tokio::time::timeout(~5s)` and cap read size (e.g. first 8–64 KB) before `classify_provider_error`. Same pattern at `openai_compat.rs:483`. |
| 3 | **Transcript corruption on a transient DB error.** `ContextBuilder::save_messages` writes ids back to in-memory dicts **only after** a successful commit (`builder.rs:383-393`); callers just `warn!` and continue (`loop_.rs:862-864, 970-972, 1327-1329, 1352-1354, 1374-1376, 1395-1397`). A `SQLITE_BUSY`/pool-acquire timeout mid-turn means the next save re-generates fresh UUIDs for the same content → **duplicate transcript rows**, and a turn ending right after a failed save silently drops its tail. | `src/agent/context/builder.rs:383-393` (assign ids into the message objects as a claim token *before* insert — dedupe in `storage/messages.rs:66-88` already skips existing ids), plus small retry on `SQLITE_BUSY` in `storage/messages.rs:55-118`. |
| 4 | **A stale direct-IP hit never falls back to the Tailscale tunnel.** `send_preferring_direct` falls back to the tunnel only on a connect-level `Err` (`openai_compat.rs:230-253`, `anthropic.rs:118-143`). The direct request has no per-request timeout (only the 30s outer header wrapper), so (a) a half-open direct connection burns the full 30s then *fails the whole call*, and (b) a stale direct IP answering 401/500 returns `Ok(resp)` (`openai_compat.rs:243-244`) → no fallback, bogus endpoint failure. | `src/provider/openai_compat.rs:69-72` (dedicated ~3-5s timeout on the direct client) + `openai_compat.rs:230-253` / `anthropic.rs:118-143` (also fall back to tunnel on timeout and on 5xx from the direct path). |
| 5 | **No retryable-vs-fatal classification; `Retry-After` never honored.** `classify_provider_error` collapses 429/500/502/503/504/529 into `Other` (`base.rs:221-265`), and the retry block retries **every** error with a fixed 1s sleep (`loop_.rs:1150-1198`, `config.rs:194-219`). A permanent 401/402/context-exceeded is therefore retried `max_retries` times and can trigger a pointless `ModelFailover` to another provider that also 401s. | `src/error.rs:64-121` (mark `AuthFailed`/`InsufficientCredits`/`ContextExceeded` non-retryable), `src/agent/loop_.rs:1150-1198` (skip retry for those; parse `Retry-After` for 429/503; jittered exponential backoff for transport errors). |

## Tier 2 — Provider health & memory safety

| # | Description | Where to change |
|---|-------------|-----------------|
| 6 | **Operational failures never mark a provider unhealthy — no circuit breaker.** Health is written only by explicit probes (`router.rs:324-395`, probed from `routes/health.rs:28-42` on status polls). After a stream dies, cached status stays `"healthy"` until the next poll+TTL, so failover re-resolution (`loop_.rs:1184`) re-picks the same broken provider. Also `get_provider_id` sorts but **never filters** unhealthy providers (`router.rs:302-321`), so `NoHealthyProvider` fires only when the map is empty. | `src/provider/router.rs:302-321` (filter to healthy when unpinned, return `NoHealthyProvider` when none), `router.rs:324-395` + `src/agent/loop_.rs:1206-1207` (passively mark unhealthy with cooldown on `ProviderError::Request`/stream `error_type=="request"`). |
| 7 | **Unbounded SSE buffers — hostile/broken provider can OOM the daemon.** `buf: Vec<u8>` grows with no max line length; `tool_call_buf`/`input_json` accumulate per tool call with no cap (`openai_compat.rs:584-585`, `anthropic.rs:601-616`). A provider streaming garbage without newlines, or unbounded tool-argument JSON, exhausts memory. | `src/provider/openai_compat.rs:584-585,881-898`, `src/provider/anthropic.rs:601-616,823-840` — cap line buffer (~1–8 MB) and per-tool accumulation (~1 MB), terminating with the existing transient-error delta. |
| 8 | **DNS resolution unbounded and IPv6-first on Android.** `resolve_dns_excluding_tailscale` (`network.rs:151-163`) has no timeout; the first non-Tailscale address (often IPv6-only on Android) is used verbatim. A stuck resolver delays every Tailscale-provider request. | `src/network.rs:151-163` — `tokio::time::timeout(~2s)` → `None`; prefer IPv4 or first connectable address; consider disabling the direct path on Android. |
| 9 | **Stale Tailscale caches.** `peers_cache` and `resolved_cache` never expire (`network.rs:50-58, 137-149`) — a network change leaves the daemon dialing stale direct IPs for its whole lifetime. | `src/network.rs:50-58,137-149` — add TTL (5–10 min) or re-fetch on miss. |
| 10 | **Dead-but-open TCP detection is slow and heartbeat-less.** Only the SSE idle timeout detects it (default 300s, `config.rs:185-190`); no `tcp_keepalive`, no total-stream-duration ceiling, and a provider dribbling 1 byte/250s keeps a turn alive indefinitely. Long for a backgrounded Android app whose sockets get killed. | `src/provider/openai_compat.rs:44-45`, `anthropic.rs:93-104` (add `tcp_keepalive`), `config.rs:185-190` (shorten idle default), and a total-stream-duration cap. |
| 11 | **Transport error taxonomy collapsed** — timeout/DNS/TLS/refused all become `ProviderError::Request{http_status:0}` (`error.rs:112-117`), so retry can't tell "DNS down, retry later" from "TLS cert invalid, never". | `src/error.rs:112-117` + `openai_compat.rs:439-451,476-480` / `anthropic.rs:431-443,467-471` — map `reqwest` `is_timeout`/`is_connect`/`is_request` into distinct variants used by the retry policy and wire tags. |
| 12 | **reqwest client build failure silently degrades to a timeout-less default client** (`openai_compat.rs:68,72`, `anthropic.rs:100,104`), silently reintroducing the exact hang the config prevents. | Same refs — log at `error` level, or treat builder failure as fatal at registration. |
| 13 | **Summarizer path has no content ceiling.** `collect_text` (`summarizer_chain.rs:156-175`) accumulates text unboundedly (it *does* read `error_type`, unlike the agent loop) and relies solely on the provider idle timeout. A runaway summarizer (compaction/title/AP-learn) grows memory; it also blocks the compaction task for the full idle window. | `src/agent/summarizer_chain.rs:156-175` — cap accumulated chars; wrap the summarizer call in an overall `tokio::time::timeout` at `summarizer_chain.rs:115-121`. |

## Tier 3 — MCP transport robustness

| # | Description | Where to change |
|---|-------------|-----------------|
| 14 | **One malformed JSON line from an MCP server kills the connection permanently.** rmcp's stdio codec maps *any* decode error to `None` → serve loop `break Closed` (rmcp `async_rw.rs:119-129`, `service.rs:613-620`); a third-party server that logs a non-JSON line to stdout takes its whole tool set offline until a manual reconnect. | rmcp dependency (`async_rw.rs:246-279`, `service.rs:613-620`) — needs `new_with_max_length` + skip-and-continue on decode errors; requires an rmcp patch, fork of `TokioChildProcess`, or hand-built `async_rw` transport. |
| 15 | **Unbounded stdout read buffer — server emitting non-newline garbage OOMs the daemon.** rmcp codec default `max_length: usize::MAX` (`async_rw.rs:157`; the guard at 355-360 is unreachable). | Same as #14 — construct the codec with a max frame length (16–64 MB). This also caps #17's pre-truncation allocation. |
| 16 | **A stalled (alive, not reading stdin) child wedges the transport permanently.** The single write mutex is held across `write.send(...).await` (rmcp `async_rw.rs:49,105-117`); on a full pipe (Windows stdin buffer ~4KB) every subsequent send queues behind it in a growing JoinSet, while the 30s timeout only aborts the caller — task leak + total loss of that server. | `src/mcp/client.rs:294-310` + a write-side `tokio::time::timeout` around `running.call_tool`/transport sends; on timeout treat the transport as dead and evict (ties to #18). |
| 17 | **Timeout abandons rather than cancels — no `notifications/cancelled`.** `client.rs:312` drops the caller future; rmcp only sends cancellation when `PeerRequestOptions::timeout` is set (`service.rs:251-277`), never here. Server-side tool keeps running → duplicate side effects when the model retries, and each timed-out call leaks a responder in rmcp's pending map (`service.rs:565-566,705`) until a (possibly never-arriving) reply. | `src/mcp/client.rs:294-310` — use `send_cancellable_request`/`PeerRequestOptions { timeout }`, or send `notifications/cancelled` manually in the timeout arm. |
| 18 | **No supervision/reconnect for stdio/SSE/in-process servers.** `connect_all` runs once (`lib.rs:153`); a crashed server stays in `servers`, its tools stay in the registry, and DB status stays `"connected"` forever (`manager.rs:37-38,131-142`; recovery only via manual PATCH at `routes/mcp.rs:324-338`). Stale tools keep being offered to the model (`manager.rs:174-196`). Only streamable_http self-heals. | `src/mcp/manager.rs` — add a daemon-side health watcher: on transport `Closed`/`TransportClosed`, mark row `error`, prune registry, reconnect with exponential backoff for enabled servers; prune `list_tools` immediately. |
| 19 | **In-process server panics are silent + `std::sync::Mutex` poisoning vector.** `client.rs:152` drops the serve task's JoinHandle unobserved; a panicking tool holding the shared embed `Arc<std::sync::Mutex<ProviderState>>` (`adaptive-pathway_rust/.../embed/provider.rs:19,50`) poisons it for every other session/path. | `src/mcp/client.rs:152` (`catch_unwind`/log the JoinHandle), audit `adaptive-pathway_rust` embed provider for `std::sync::Mutex` → `tokio::sync::Mutex` or `into_inner()` on poison. |
| 20 | **Hand-rolled `SseTransport` skips id correlation** — a reply with a wrong `id` or a 200 with no body is reported as *success* with empty content (`sse_transport.rs:97-132,184-198`); notification errors dropped; invalid headers silently discarded (`sse_transport.rs:48-61`). | `src/mcp/sse_transport.rs:97-132` — validate response id, treat missing `result`/`error` as protocol error, surface notification `error` bodies, warn on dropped headers. |
| 21 | **Tool-call timeout hardcoded at 30s** (`manager.rs:15,208`), not configurable per server; a legitimately long tool is always cut. | `src/models/mcp.rs:25-46` (+ `MCPServerConfig`), `src/mcp/manager.rs:15,208`, `config.rs` — plumb optional `timeout_s` with the current default as fallback. |
| 22 | **Huge tool results are fully materialized before the 100KB truncation** (`client.rs:314-315` join-then-truncate; `tools.rs:25-70`), and `output_size_bytes = content.len() as i32` can overflow negative past 2GB (`models/mcp.rs:80`). | `src/mcp/client.rs:314-315`, `tools.rs:25-70` — stream-truncate during extraction; clamp to `i32::MAX`. |
| 23 | **`connect_server` race can transiently double-spawn a child** (no per-id in-flight guard; `/connect` + PATCH can race, `manager.rs:54-114`, `routes/mcp.rs:324-365`). | `src/mcp/manager.rs:54-114` — serialize connects per server id. |

## Tier 4 — Turn lifecycle, HITL, scheduler, SSE

| # | Description | Where to change |
|---|-------------|-----------------|
| 24 | **Aborting a turn while paused on HITL approval leaks a stale approval + a `Notify`.** `handle.abort()` (user cancel, disconnect watcher, or shutdown) unwinds at the `notify.notified()` await (`loop_.rs:1950`), skipping the cleanup at `loop_.rs:1953-1963`. The `PendingAction` then lingers until the next `create_pending`-triggered `sweep_stale` (`hitl/manager.rs:249-277`), and `GET /api/chat/{id}/pending` keeps advertising an approval no waiter honors. | `src/agent/mod.rs:347-380` (`cancel`/`cancel_if_current` — call `hitl.cancel_pending(session_id)` and drain `hitl_notifies`), `src/routes/chat.rs:136-150` (`delete_session` too), plus call `sweep_stale` from `get_pending_approvals`. |
| 25 | **Disconnect-watcher abort leaves the session row stuck at `status='active'`.** Only the explicit `/cancel` route resets to `idle` (`chat.rs:412`); `cancel_if_current` (`mod.rs:372-380`) aborts without touching `sessions.status`. A dropped mobile connection leaves a permanently-running session. | `src/agent/mod.rs:372-380` — after `emit_cancelled`, fire-and-forget `sessions::update_session_status(..., "idle")`. |
| 26 | **Scheduled jobs have no overlap guard.** tokio-cron-scheduler spawns a fresh task per due tick with no in-flight check; a 10-min `*/5` recipe job spawns overlapping executions (concurrent provider spend, interleaved `execution_history` rows). | `src/scheduler/mod.rs:87-94,282-367` — per-job in-flight set (DashMap), set at the top of `execute_job`, cleared on every exit path including early returns at 296-317. |
| 27 | **Failed scheduled runs leak a temp session + its message batch forever** (success path deletes its session at `scheduler/mod.rs:343`; failure path keeps it as FK anchor at 345-365, and nothing prunes it). | `src/scheduler/mod.rs:345-365` — after recording the `failed` row, delete the temp session's messages; make `execution_history.session_id` nullable (migration) or re-point to a reserved `_failed_jobs` anchor. |
| 28 | **No retention/pruning anywhere** — `llm_timings` (insert-only, `timings.rs`), `execution_history`, and old session messages grow unbounded; WAL file grows with no checkpoint. On a phone-sized partition this eventually hits `SQLITE_FULL` → 500s on every route while `/api/health` still reports healthy. | `src/storage/mod.rs:30-73` — retention sweep at boot + daily: prune old `execution_history`/`llm_timings` (by `idx_execution_trigger`), cap messages per session, opportunistic `wal_checkpoint(TRUNCATE)`. |
| 29 | **Pool/busy-timeout left at library defaults** (10 connections, 30s acquire, **5s** busy_timeout, `synchronous=FULL` fsync per commit, `storage/mod.rs:46-69`) — poor fit for Android's throttled storage under the write burst pattern (tool calls + timings + metadata after every step). | `src/storage/mod.rs:46-69` — explicit `max_connections(~4-5)`, `busy_timeout(10-30s)`, `synchronous=NORMAL` (crash-safe against app kill in WAL), explicit acquire policy. |
| 30 | **Legacy-Python bootstrap inserts `_sqlx_migrations` rows outside a transaction** (`storage/mod.rs:135-146`, autocommit per row). A crash mid-bootstrap leaves a partial table that makes the *normal* sqlx migrator re-apply already-done `ALTER TABLE`s on next boot → "duplicate column name" → daemon refuses to start. | `src/storage/mod.rs:129-146` — wrap the insert loop in `BEGIN IMMEDIATE ... COMMIT` on the checked-out connection. |
| 31 | **SSE channel is unbounded — no backpressure on a slow/dead client.** `mpsc::unbounded_channel` (`chat.rs:558-567`); bounded today only by the `is_closed()` break (`loop_.rs:953`) and the ≤30s watcher abort. A slow webview can pile up a turn's tail of events in RAM. | `src/routes/chat.rs:558-567` — `mpsc::channel(1024)` with `try_send`/drop-oldest policy (keep unbounded only if a cap is added in `run_turn`). |

---

## Progress

| Tier | Status | Closed | Notes |
|------|--------|--------|-------|
| 0 — Upgrade safety | ✅ Done | #30 | Transaction-wrapped legacy bootstrap; regression test `connect_heals_a_partial_legacy_bootstrap`. |
| 1 — Data loss / hangs / unbounded retry | ✅ Done | #1, #2, #3, #4, #5 | Mid-stream errors route through shared retry budget; bounded error-body read; transcript idempotency; direct/tunnel fallback; retryable-vs-fatal classification + `Retry-After` + jittered backoff. Frontend `error_type` tags unchanged (still consumed). |
| 2 — Provider health & memory safety | ✅ Done | #6, #7, #8, #9, #10, #11, #12, #13 | Passive circuit breaker (`mark_unhealthy` + cooldown, `is_transport_error`); SSE line/tool caps; DNS 2s timeout + IPv4-first; Tailscale cache TTLs (10m/5m); `tcp_keepalive` + idle default 300→120s + 1h stream cap; transport taxonomy (`ConnectFailed`/`Timeout`/`Request`); client-builder failure logged at `error`; summarizer 200k-char ceiling + 300s overall timeout. |
| 3 — MCP transport robustness | ⬜ Not started | — | rmcp 2.x decision gate first (#14–#23). |
| 4 — Turn lifecycle / HITL / scheduler / SSE | ⬜ Not started | — | #24–#29, #31. |

Checkpoints:
- Tier 0+1 checkpoint: `cargo test` 330 unit + 30 integration green, `cargo clippy --all-targets` clean, `pnpm test` 272 green, `pnpm lint` fails only on pre-existing Prettier drift (no eslint errors).
- Tier 2 checkpoint (2026-08-20): `cargo test` **350 unit + 30 integration, 0 failures**, `cargo clippy --all-targets` **clean**.
- **Android gating check still blocked:** `cargo ndk -t arm64-v8a --platform 26 check --lib` (in `src-tauri/`) requires `ANDROID_NDK`/`ANDROID_NDK_ROOT` env, which is unset in this environment (cargo-ndk present). Must run before Tier 2 can be considered fully gated on Android.

---

## Already solid (do not regress)

- Crash-safe normal migration path + `_sqlx_migrations` recorded in-transaction.
- FK enforcement per-connection; `BEGIN IMMEDIATE` write transactions (no upgrade deadlocks).
- Compaction CAS lock with stale reclaim (`compaction.rs:779-790`).
- Turn-slot reservation via `DashMap::entry` + `TurnCleanup` drop guard (`mod.rs:217-220,77-89`) — no double-spawn, no stale-abort, cleanup even on panic.
- Idempotent message dedupe per id.
- Constant-time auth comparison; `require_secret` fails *closed* on Android (`middleware.rs`).
- Bounded HITL wait (`HITL_APPROVAL_TIMEOUT`) and lost-wakeup-free Notify registration order.
- Never-throws `execute_tool` contract (verified by `tests/mcp_never_throws.rs`).
- Zombie-free child reaping and 3s-grace graceful shutdown (rmcp `child_process.rs`).
- 100KB tool-result truncation; connect-time schema hardening (`jsonschema::validator_for`).
- Per-chunk SSE idle-read timeout; content-ceiling backstop (`MAX_TURN_CONTENT_CHARS`).

---

## Suggested order of attack

1. **#1 → #2 → #3** — correctness/hangs (mid-stream retry, bounded error-body read, transcript idempotency).
2. **#14/#15 → #16/#17** — MCP memory safety & wedge/cancel (largest chunk: requires an rmcp upgrade/patch or a hand-built `async_rw` stdio transport; also caps the pre-truncation allocation).
3. **#6 → #4 → #5** — provider health/failover quality.
4. **#24–#31** — turn lifecycle, HITL cleanup, scheduler guards, retention.
5. **#7–#13, #19–#23** — hardening batch (buffer caps, DNS, keepalive, taxonomy, in-process panic containment, SSE transport validation).

## Verification

- `cargo test` (unit + `tests/`), `cargo clippy` in `plugins/bigtiny_rust/`.
- `pnpm test` / `pnpm lint` at repo root only if frontend-visible behavior changes (e.g. new `error_type` wire tags).
- Android lane: `cargo ndk -t arm64-v8a --platform 26 check --lib` in `src-tauri/` (the gating check — plain `cargo check --target` silently skips `llama-cpp-sys-2`).

---

## Verification Report (2026-08-19)

All 31 findings re-verified against the current `plugins/bigtiny_rust` sources plus vendored
`rmcp` 0.9.1 and `tokio-cron-scheduler` 0.13.0. Every finding is **confirmed**; the table
below records line-ref corrections, scope caveats, and risks already mitigated in the code.
Nothing here was edited into source — this is an audit report only.

| # | Status | Corrected / verified detail |
|---|--------|----------------------------|
| 1 | ✅ Confirmed | `Delta.error_type` at `provider/base.rs:20`; never read in `process_stream` (`agent/loop_.rs:1640-1755`). Error deltas emitted at `openai_compat.rs:902-930`, `anthropic.rs:842-866`. Non-`stop` finish → `step+=1; continue` at `loop_.rs:1370-1381`. Retry/failover block (`loop_.rs:1090-1201`) wraps only pre-stream `Err` — mid-stream errors bypass it entirely. |
| 2 | ✅ Confirmed w/ caveat | `resp.text().await` unbounded at `openai_compat.rs:455` / `anthropic.rs:447`. **Correction:** the cited "same pattern" at `openai_compat.rs:483` / `anthropic.rs:474` is *bounded* — the request carries `.timeout(5s)` (`openai_compat.rs:473`, `anthropic.rs:464`) which covers the body. Only the chat-completion error-body read is unbounded. |
| 3 | ✅ Confirmed | `builder.rs:383-393` writes generated ids back only after `save_messages` succeeds; callers `warn!` and continue (`loop_.rs:1352-1354,1374-1376,1395-1397`). Dedupe exists in `storage/messages.rs:66-88` — the claim-token fix is viable. |
| 4 | ✅ Confirmed | `send_preferring_direct` returns the direct `Ok(resp)` even on 401/500 (`openai_compat.rs:243-244`, `anthropic.rs:132-133`); fallback only on connect `Err`. Direct client has connect-only timeout (`openai_compat.rs:69-72`, `anthropic.rs:101-104`); the 30s `RESPONSE_HEADERS_TIMEOUT` (`openai_compat.rs:53`, `anthropic.rs:81`) bounds the attempt but not the body. |
| 5 | ✅ Confirmed w/ nuance | `classify_provider_error` (`base.rs:221-265`) **does** split 401/403→`AuthFailed`, 402/billing→`InsufficientCredits`, context→`ContextExceeded`; 429/500/502/503/504/529 collapse into `Other`. Fixed 1s sleep (`loop_.rs:1175-1178`). **Correction:** `FallbackConfig.enabled` defaults to **false** (`config.rs:194-219`, delay 1000, retries 2) → `max_attempts = 1` (`loop_.rs:1084-1088`), so there is *no* pre-stream retry at all by default, and the "401 retried max_retries times" scenario requires `enabled=true`. |
| 6 | ✅ Confirmed | `get_provider_id` sorts unhealthy last but never filters (`router.rs:302-322`); health written only by probes (`router.rs:324-359`, `routes/health.rs:28-44`). `NoHealthyProvider` only fires when the map is empty. |
| 7 | ✅ Confirmed | Unbounded `buf: Vec<u8>` (`openai_compat.rs:584`/`anthropic.rs:614`), unbounded per-tool accumulation (`openai_compat.rs:580`, `anthropic.rs:610`). |
| 8 | ✅ Confirmed | `resolve_dns_excluding_tailscale` (`network.rs:151-163`), no timeout, first non-Tailscale addr used (IPv6-first possible). |
| 9 | ✅ Confirmed | `peers_cache` (`network.rs:40,50-58`) and `resolved_cache` (`network.rs:41,137-149`) never expire. |
| 10 | ✅ Confirmed | No `tcp_keepalive` (`openai_compat.rs:60-68`, `anthropic.rs:93-104`); idle default 300s (`config.rs:185-190`); no total-stream cap. |
| 11 | ✅ Confirmed | Transport failures → `ProviderError::Request{http_status:0}` (`openai_compat.rs:439-451,476-480`, `anthropic.rs:431-443,467-471`); no `is_timeout`/`is_connect` discrimination. |
| 12 | ✅ Confirmed | `.build().unwrap_or_default()` at `openai_compat.rs:60-72` and `anthropic.rs:93-104`. |
| 13 | ✅ Confirmed | `collect_text` (`summarizer_chain.rs:156-175`) accumulates unbounded text; reads `error_type` (line 167); router call at 115-119 has no total timeout (SSE idle only). |
| 14 | ✅ Confirmed w/ nuance | rmcp `async_rw.rs:119-129` maps any decode error to `None` → serve loop breaks `Closed` (`service.rs:613-620`). **Caveat:** 0.9.1 *already* skips valid non-standard JSON notifications (`async_rw.rs:246-279`, `should_ignore_notification` 223-243) — only malformed/non-JSON lines kill the transport. Doc's claim is right for garbage, overstated for valid JSON log lines. |
| 15 | ✅ Confirmed | Default `max_length: usize::MAX` (`async_rw.rs:157`); `new_with_max_length` exists (162-167) but BigTiny never uses it. Guard at 355-360 unreachable with the default. |
| 16 | ✅ Confirmed | Write mutex held across `send().await` (`async_rw.rs:105-117`); sends run as `JoinSet` tasks (`service.rs:580,692-697,706-714`, 64-slot sink proxy 555-557). BigTiny's 30s outer timeout (`client.rs:294-312`) drops the caller without draining the pipe. |
| 17 | ✅ Confirmed | `call_tool` (rmcp `client.rs` macro 355) → `send_request` (no options, `service.rs:371-376`) → `await_response` sends no cancellation (`service.rs:251-277`). Outer timeout leaks the `local_responder_pool` entry (`service.rs:565-566,700-705`; only removed on response/error/cancel at 823-837). No `notifications/cancelled`. |
| 18 | ✅ Confirmed w/ nuance | `connect_all` runs once (`lib.rs:153`; `manager.rs:146-166`), no supervision. **Caveat:** `recipes/engine.rs:201` calls `connect_server` per recipe run — there's reconnect-on-demand, but no daemon-side health watcher; stale tools stay registered on transport death (only `prune_registry_for` on explicit reconnect, `manager.rs:121-123`). |
| 19 | ✅ Confirmed | `client.rs:152` drops the in-process serve `JoinHandle`. `adaptive-pathway_rust/src/embed/provider.rs:50` uses `Arc<Mutex<ProviderState>>` with `.lock().unwrap()` (103, 111, 139) — poisoning vector confirmed. |
| 20 | ✅ Confirmed w/ corrections | `sse_transport.rs:97-132` never correlates response `id` (generated 98, never checked); missing `result` → `json!({})` success (131); `call_tool` missing `content` → empty success (196). **Corrections:** notification non-2xx is now surfaced (`send_notification` 134-154), not dropped as the doc claims; invalid headers silently discarded at 48-61 (confirmed). |
| 21 | ✅ Confirmed | `DEFAULT_TOOL_TIMEOUT = 30s` (`manager.rs:15`); default applied at `manager.rs:208`; agent passes `None` (`loop_.rs:2013`); no per-server timeout in `MCPServerConfig` (`models/mcp.rs:25-46`). |
| 22 | ✅ Confirmed | Full materialization then truncation (`client.rs:312-324`); `content.len() as i32` overflow at `client.rs:314`; `truncate_output` at `tools.rs:12-19`. |
| 23 | ✅ Confirmed | No per-id in-flight guard in `connect_server` (`manager.rs:54-114`); PATCH + `/connect` can race (`routes/mcp.rs:324-338,357-365`), plus recipe engine can connect concurrently. |
| 24 | ✅ Confirmed | `cancel`/`cancel_if_current` (`agent/mod.rs:347-352,372-380`) abort without cleaning `hitl` pending or `hitl_notifies`. `cancel_pending`/`remove_pending` exist (`hitl/manager.rs:388-394,402-412`) but are only reached via the approval-timeout path (`loop_.rs:1950-1967`). `delete_session` (`chat.rs:136-150`) doesn't clean them either. |
| 25 | ✅ Confirmed | `/cancel` sets `idle` (`chat.rs:412`); `cancel_if_current` (`mod.rs:372-380`) doesn't touch `sessions.status`. |
| 26 | ✅ Confirmed | tokio-cron-scheduler 0.13.0 `JobRunner` (`runner.rs:46-59`) holds the job lock only while *building* the future, then spawns it; next ticks derive from the cron schedule (`scheduler.rs:156-223`), not execution end. No in-flight guard → overlapping runs. (BigTiny's `_lock` closure param is the `JobsSchedulerLocked` handle, not a per-job lock.) |
| 27 | ✅ Confirmed | Success path deletes temp session (`scheduler/mod.rs:343`); failure path keeps it as FK anchor (345-365), never pruned. |
| 28 | ✅ Confirmed | No retention/delete anywhere for `llm_timings`/`execution_history`/`messages`; `timings.rs` insert-only; `storage/mod.rs:30-73` has no checkpoint/retention sweep. |
| 29 | ✅ Confirmed | `storage/mod.rs:46-49,64` sets only filename/create/foreign_keys; no `busy_timeout`, `max_connections`, or `synchronous` override → sqlx defaults (10 conn, 30s acquire, 5s busy, FULL sync). |
| 30 | ✅ Confirmed | `bootstrap_legacy_python_schema` (`storage/mod.rs:102-149`): loop inserts `_sqlx_migrations` rows one-by-one (135-146) with no transaction. Mid-loop crash → partial table → `already_bootstrapped > 0` early-returns (112-119) → `sqlx::migrate!` re-applies remaining `ALTER TABLE` → "duplicate column name" → daemon won't start. `BEGIN IMMEDIATE…COMMIT` fix is valid. |
| 31 | ✅ Confirmed | `mpsc::unbounded_channel` at `chat.rs:558`. |

### Newly identified (not in the original doc)

- **Mid-stream provider error events are silently swallowed** by both parsers — OpenAI has no top-level `error` handling; Anthropic ignores the `error` event. They surface only as an empty `finish_reason="error"` delta, feeding the #1 unbounded step-retry.
- **HITL abort leaks are wider than stated:** `hitl_notifies` (DashMap of `Arc<Notify>`) is also never drained on cancel, so an aborted wait leaves both a stale `PendingAction` *and* a parked `Notify` keyed by `action_id` until the next `create_pending` sweep.
- **`delete_session` cancels the turn but not session-scoped HITL state** — the `session_pending` list for a deleted session can retain action ids pointing at already-removed pendings.

### "Already solid" list — verified still accurate

Crash-safe normal migration path, per-connection FK, `BEGIN IMMEDIATE` writes, compaction CAS lock,
`DashMap::entry` turn-slot reservation, message-id dedupe, constant-time auth, bounded HITL wait with
register-before-notify ordering, never-throws `execute_tool`, rmcp child reaping/graceful shutdown,
100KB truncation + connect-time schema compile check, per-chunk SSE idle timeout, and
`MAX_TURN_CONTENT_CHARS` backstop — all still present in the code.

### Recommended execution order update

Do **#30 first** (one small transaction wrapper, prevents a bricked upgrade), and treat the SSE
error-event swallowing as part of **#1** rather than a separate item.

---

## Implementation Report (2026-08-20)

Tier 0 (#30), Tier 1 (#1–#5), and Tier 2 (#6–#13) are implemented and gated green
locally (`cargo test` 350 unit + 30 integration, 0 failures; `cargo clippy --all-targets`
clean). This report records what changed per finding, the key decisions, and the one
remaining gating gap (Android NDK env).

### Tier 0
- **#30** `storage/mod.rs` — legacy-Python bootstrap now seeds `_sqlx_migrations`
  inside `BEGIN IMMEDIATE … COMMIT` on the checked-out connection; added a
  completeness probe (re-apply only `version <= max_version` rows) and regression test
  `connect_heals_a_partial_legacy_bootstrap`.

### Tier 1
- **#1** `process_stream` returns `Result<_, ProviderError>`; mid-stream top-level
  `error` object (OpenAI) / `error` event (Anthropic) → transient-error delta; shared
  retry budget routes stream errors through the same retry/failover block.
- **#2** `read_bounded_error_body` (base.rs) — 5s timeout + 8KB cap on non-2xx bodies.
- **#3** `save_messages` claims ids into the message objects *before* insert (dedupe in
  `storage/messages.rs` skips existing ids); `SQLITE_BUSY` retried 3×/50ms; test
  `failed_save_reverts_claimed_ids_so_a_retry_can_persist`.
- **#4** `try_direct` helper in both providers — direct attempt bounded by
  `DIRECT_HEADERS_TIMEOUT` (5s, headers only); falls back to tunnel on timeout / transport
  error / **any** non-2xx from a stale direct IP. Tests: direct 500/401/silent-address all
  fall back.
- **#5** `ProviderError::{is_retryable, retry_after}`; `Other.retry_after_secs`;
  `parse_retry_after`; jittered exponential backoff (lock-free xorshift64, 60s cap)
  honoring `Retry-After`; non-retryable (`AuthFailed`/`InsufficientCredits`/`ContextExceeded`)
  fail fast. `classify_provider_error` takes `(status, body, retry_after)`.

### Tier 2
- **#6** `router.rs` `get_provider_id(None)` filters out `unhealthy` (keeps `disconnected`
  selectable); `mark_unhealthy(id, reason)` sets unhealthy + `health_checked_at = now`
  (cooldown = health TTL); `loop_.rs` retry arm calls it on transport errors via the new
  `ProviderError::is_transport_error()`. 4 router tests.
- **#7** `MAX_SSE_LINE_BYTES` (8MB) + `MAX_TOOL_ARGUMENTS_BYTES` (1MB) in both SSE
  streams; overlong line / overlong tool args → transient-error delta. 3 tests.
- **#8** `resolve_dns_excluding_tailscale`: `tokio::time::timeout(2s)` → `None`; IPv4-first
  via factored `pick_direct_address` (unit-tested). 4 network tests.
- **#9** `peers_cache` TTL 10m, `resolved_cache` TTL 5m; re-resolve on stale/missing peer,
  drop stale resolved entry when peer is gone. 2 network tests.
- **#10** `tcp_keepalive(30s)` on both main + direct clients (both providers); idle default
  300s → 120s (`config.rs` + tests); `MAX_STREAM_DURATION` (1h) cap enforced in both
  streams' `poll_next`. 2 stream-cap tests.
- **#11** New `ProviderError::ConnectFailed` / `Timeout` variants; `classify_transport_error`
  maps reqwest `is_connect`/`is_timeout`/`is_request`; all three → wire tag
  `network_unreachable`, retryable, `is_transport_error()`; loop breaker catches all three.
  3 base.rs tests (incl. real refused-port → `ConnectFailed`, stalled peer → `Timeout`).
- **#12** Both providers' client builders no longer `unwrap_or_default()` silently — on
  build failure they `tracing::error!` and fall back to `reqwest::Client::new()`.
- **#13** `collect_text` caps accumulated chars at `MAX_SUMMARIZER_TEXT_CHARS` (200k);
  `via_router` wraps the whole call in `tokio::time::timeout(300s)`. 2 tests.

### Frontend
Unchanged: the four `error_type` tags (`network_unreachable`, `auth_failed`,
`context_exceeded`, `insufficient_credits`) are already consumed at
`src/stores/chat/errorUtils.ts` / `chatStore.ts` / `src/lib/types.ts`.

### Open decisions / caveats
- Idle default lowered to 120s (was 300s). Slow local models can override via
  `idle_timeout_secs`. `tcp_keepalive` + 1h stream cap make the tighter value safe.
- Android direct-LAN path left enabled (not disabled) — IPv4-first + 2s DNS timeout +
  tunnel fallback already neutralize the "unreachable IPv6 direct address" risk; disabling
  the path outright would remove a useful optimization with no remaining safety upside.
- **Android gating check not run:** `ANDROID_NDK`/`ANDROID_NDK_ROOT` unset in this env.
  All Tier 2 code is `#[cfg]`-neutral and exercises no Android-only APIs, but the gating
  `cargo ndk -t arm64-v8a --platform 26 check --lib` must still pass before declaring
  Tier 2 fully closed on Android.

### Next
Tier 3 (#14–#23) — rmcp 2.x decision gate first.