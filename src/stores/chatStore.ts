// Chat render state. Assembled live from `chat://*` events; the durable
// conversation lives in goosed (CLAUDE.md rule 3). The assembly is turn-aware so
// it handles both live prompting and full-conversation replay on session/load.

import { create } from 'zustand';
import {
  ipc,
  onAdoptSession,
  onApprovalNeeded,
  onChatError,
  onClipboardAttach,
  onCompaction,
  onComplete,
  onMessageDelta,
  onProviderActivated,
  onProviderHealth,
  onReasoningDelta,
  onRecipesChanged,
  onSessionDeleted,
  onSessionsCleared,
  onSessionTitle,
  onToolCall,
  onUserMessage,
  pickSavePath,
} from '@/lib/ipc';
import { buildExport, sanitizeFilename } from '@/lib/chatml';
import { defaultSystemPrompt } from '@/lib/system_prompts';
import { recipeNeedsAttention, resolveRecipe, launchableExtensions } from '@/lib/recipes';
import { supportsImages } from '@/lib/vision_models';
import type {
  ApprovalNeededEvent,
  ModeInfo,
  NetworkTier,
  PathInfo,
  ProviderView,
  Recipe,
  SessionInfo,
  ThinkingEffort,
  ToolCallUpdate,
} from '@/lib/types';

import { decideChatApproval, pickRejectOption } from './chat/approvalUtils';
import {
  buildStrippedTranscript,
  stripPromptPreamble,
  stripRecipeWrapper,
  stripInternalMarkers,
} from './chat/errorUtils';
import {
  countToolCall,
  exceedsReasoningCap,
  FORCED_ANSWER_PROMPT,
  hasRepetitionLoop,
  splitLeakedThinkTag,
  TOOL_LOOP_THRESHOLD,
  type ToolCallCounts,
} from './chat/loopGuards';
import {
  closeOpen,
  deriveArtifact,
  extractGooseToolMeta,
  findMatchingProvider,
  isImageFileName,
  isStragglerAssistantMessage,
  userFileArtifact,
} from './chat/messageUtils';
import { readCachedModeInfo, writeCachedModeInfo } from './chat/modeInfoCache';
import type { Artifact, Attachment, Message, PendingImage, ToolCall } from './chat/types';

export * from './chat/approvalUtils';
export * from './chat/errorUtils';
export * from './chat/loopGuards';
export * from './chat/messageUtils';
export * from './chat/modeInfoCache';
export * from './chat/types';

interface ChatState {
  sessionId: string | null;
  cwd: string | null;
  /** Monotonic session-switch counter (WS8): bumped at the START of every
      session-changing action (newSession/loadSession), so an in-flight
      `loadSession` replay that finishes after the user already moved on to a
      new session can detect it's stale (`epoch !== get().sessionEpoch`) and
      skip applying its captured replay state — otherwise "New Chat" clicked
      mid-replay gets clobbered by the old session's completion set. */
  sessionEpoch: number;
  /** A session this window abandoned mid-turn whose generation is still
      running in the background (WS8: we deliberately keep background turns
      running when the user moves on). Non-null → a subtle "still running"
      indicator renders; cleared when that turn's `chat://complete`/`chat://error`
      arrives (surfacing a completion toast instead). */
  backgroundSession: { sessionId: string; cwd: string; title: string | null } | null;
  /** Completion toast for a backgrounded turn (see `backgroundSession`) — the
      user gets told their abandoned chat finished and can jump back to it.
      `ok` distinguishes a clean completion from a failure. */
  backgroundTurnToast: {
    sessionId: string;
    cwd: string;
    title: string | null;
    ok: boolean;
  } | null;
  /** The session's original chat folder, captured once at creation/load time
      — used (alongside `cwd`) by the agentic-mode client-side approval
      nicety (`onApprovalNeeded`) so an in-bounds call isn't needlessly
      queued for a human decision after "Set as working directory" has
      diverged `cwd` away from it. This is purely a UX optimization, not a
      security boundary (BigTiny enforces the real containment) — if a
      resumed session had already diverged in an earlier window session,
      this may capture the diverged `cwd` instead of the true original
      chat_dir; the only consequence is a few more real approval prompts
      than strictly necessary, never a security gap. */
  chatDir: string | null;
  title: string | null;
  mode: string | null;
  availableModes: ModeInfo[];
  /** Reasoning-effort control for the active session (Round-7) — `null` when
      the active model doesn't support effort control at all (a single-option
      "off"-only model, per `parse_thinking_effort` in commands/session.rs).
      Live, per-session, no goosed restart. */
  thinkingEffort: ThinkingEffort | null;
  /** True only during the async gap in `newSession()` between the optimistic
      clear and the real `session/new` response landing (Round: header-delay
      fix). `mode`/`availableModes`/`thinkingEffort` are deliberately *not*
      cleared during this gap (they keep showing the outgoing session's
      values, which are usually still correct for a fresh session on the same
      provider) — this flag instead gates *interactivity* on `ModeBadge`/
      `EffortDropdown` so a click can't act on a session id that doesn't
      exist yet. */
  creatingSession: boolean;
  messages: Message[];
  artifacts: Artifact[];
  droppedFiles: PathInfo[];
  attachments: Attachment[];
  pendingImages: PendingImage[];
  /** Raw paths currently being inspected/read by `addDroppedPaths`, before
      they land in `droppedFiles`/`attachments`/`pendingImages` — drives a
      placeholder "attaching…" chip so a large file or a cold session-create
      (chat-only mode's binary-file path needs a real session first) doesn't
      look like nothing happened. Cleared (per-path) once that file's own
      processing finishes, success or failure. */
  pendingAttachments: string[];
  pendingApprovals: ApprovalNeededEvent[];
  busy: boolean;
  /** The goosed-level `providerId`/`modelId` (from `session/list`'s `_meta`,
      see `SessionSummary`) the CURRENTLY loaded session was last used with —
      not necessarily a Kitty provider profile id, since goosed has no
      concept of Kitty's multi-profile abstraction. `null` when unknown
      (a session predating this metadata) or not applicable (brand-new
      session, always on the currently-active provider). Kept in state
      (not just a one-off `loadSession` param) so `reloadCurrent()` can
      recompute `sessionConcluded` without the caller re-supplying it. */
  sessionProviderId: string | null;
  sessionModelId: string | null;
  /** True when the loaded session's stored provider/model (above) doesn't
      match ANY current Kitty provider profile — e.g. the profile was
      deleted since this chat was last used. The composer disables and shows
      "Chat concluded." instead of blocking on a provider Kitty can't
      restore. History still replays normally; only new sends are blocked. */
  sessionConcluded: boolean;
  /** True only while `loadSession` is actively replaying a resumed
      conversation (Round-7 perf fix). The message list renders a lightweight
      placeholder instead of the real list while this is true — a long
      session's replay fires one `chat://*` event per historical turn/tool-call
      with no batching, and rendering (and re-scrolling) the growing list on
      every single one of those was the actual bottleneck, not the replay
      itself (goosed streams the whole thing before `session/load`'s ACP
      response even returns). `messages` still accumulates normally underneath;
      nothing subscribes to render it until this flips back to `false`, so the
      final render is one clean paint instead of hundreds of incremental ones. */
  replaying: boolean;
  error: string | null;
  /** Structured classification of `error`, set only by `onChatError` (a
      BigTiny `provider_error` event) — e.g. "context_exceeded" |
      "insufficient_credits". Kept separate from `error` rather than
      changing its shape, since ~30 call sites set `error: String(e)` from
      plain catch blocks and have no classification to offer. */
  errorType: string | null;
  // Active-provider derived state (Phase 9/10)
  providerTier: NetworkTier | null;
  providerHost: string | null;
  providerOffline: boolean;
  /** True while a manual "Retry connection check" (ChatView's banner) is in flight. */
  checkingConnection: boolean;
  isTrusted: boolean;
  model: string | null;
  /** Active provider's display name, for the per-response metrics line (Round-3 item 2). */
  providerName: string | null;
  /** STOPGAP client-side workaround (see `send()`) — strip reasoning from the
      context resent on later turns, chat-only mode only. Remove once Goose ships
      a native hook (block/goose#7617) and thread it into goosed_env() instead. */
  stripReasoning: boolean;
  /** Active provider's custom system prompt override, or `null` to use the
      built-in mode-appropriate default (`defaultSystemPrompt`). Prepended to a
      session's first outgoing message only — see `send()`. */
  systemPrompt: string | null;
  /** Per-session chat/agentic override (Round-4 instant mode toggle). `null` =
      default to chat (owner ask — see `isChatMode`). */
  modeOverride: 'chat' | 'agentic' | null;
  /** Approval mode saved when flipping into chat mode, restored on flip back
      (Round-4 tool-safety fix). `null` when not currently overridden-to-chat. */
  savedApprovalMode: string | null;
  /// Non-blocking notice (e.g. attaching to an untrusted provider). Round-2 item 13.
  warning: string | null;
  /** BigTiny folded older history into its background memory summary
      (`chat://compaction`, from `bigtiny/agent/compaction.py`). Purely
      informational — nothing to act on, just lets the user know context
      isn't silently growing forever. */
  compactionNotice: string | null;
  /** Stop/Force-Stop phase (Round-5). `null` = not stopping. `'stopping'` =
      Stop was clicked, cancel notification sent, waiting out the grace period
      for goosed to actually end the turn. `'forceable'` = grace elapsed with no
      completion, so the user can Force-Stop to hard-reset. */
  stopPhase: 'stopping' | 'forceable' | null;
  /** Session whose in-flight turn was force-stopped (Round-5): late stream
      events for it are ignored (see `forActive`) until the next `send()` starts
      a fresh turn, so the UI doesn't resume "typing" after a force-stop. */
  abandonedSession: string | null;
  /** True once `hasRepetitionLoop` detects the current turn looking stuck in a
      repetition loop — surfaced as a suggestion to cancel, never an automatic
      cancel (the user decides). Reset when a fresh turn starts or the turn
      ends. */
  loopSuspected: boolean;
  /** Recipe templates (Goose recipes reinterpreted as client-side templates —
      see `sendWithRecipe`) — kept fresh via `bindEvents`'s `onRecipesChanged`
      subscription, read by the composer's slash-command matching. */
  recipes: Recipe[];
  /** Set by `sendWithRecipe`, consumed exactly once by `send()`'s prompt
      construction (any turn, not just a session's first message — a recipe
      can be invoked at any point in a conversation). `null` the rest of the
      time. */
  pendingRecipeCard: { title: string; instructions: string; maxReasoningTokens: number } | null;
  /** Set alongside `busy: true` for a turn that came from `sendWithRecipe`
      (derived from `pendingRecipeCard` right before it's consumed), cleared
      at every turn-end/turn-reset site (same lifecycle as `loopSuspected`).
      Two effects while set: (1) `flushDeltas` skips the repetition-loop
      suggestion for this turn — a recipe (e.g. the debate moderator) can
      legitimately produce long, structurally-repetitive output that would
      otherwise false-positive; (2) `flushDeltas` enforces
      `maxReasoningTokens` as a hard cap, auto-cancelling the turn if
      exceeded — see that function for why this needs its own enforcement
      instead of just suppressing the loop suggestion. */
  activeRecipeTurn: { maxReasoningTokens: number } | null;
  /** Set when the reasoning-cap cancel above fires, naming the session the
      forced follow-up belongs to. There's no ACP way to interrupt just the
      reasoning phase and redirect a generation already in flight to its
      final answer — cancelling ends the whole turn — so instead, once the
      cancelled turn's completion/error event actually arrives (`onComplete`/
      `onChatError`), a follow-up asking the model to answer now is sent
      automatically, so the user gets a response instead of nothing. Cleared
      once consumed, and by `forceStop()` (an explicit Force Stop overrides
      this — no surprise follow-up after the user deliberately kills a turn). */
  pendingForcedAnswer: string | null;
  bindEvents: () => void;
  dismissWarning: () => void;
  dismissCompactionNotice: () => void;
  /** Dismiss the backgrounded-turn completion toast (see `backgroundTurnToast`). */
  dismissBackgroundToast: () => void;
  /** Manually compact the current session's context (`/compact`): forces the
      daemon to fold the oldest un-compacted exchanges into its memory slots,
      regardless of the automatic token threshold, then surfaces a notice with
      what was folded. No-op when there's no active session. */
  compact: () => Promise<void>;
  /** Drop any artifact whose file no longer exists on disk — covers both a
      tool call deleting it and the user deleting it out-of-band (Explorer,
      another app). Best-effort: a failed check just leaves the list as-is
      until the next call. */
  pruneMissingArtifacts: () => Promise<void>;
  /** Scan `cwd` on disk and merge in any file not already tracked as an
      artifact (e.g. dropped in via Explorer rather than written by a tool
      call) — the tool-call-derived path above is event-driven and only ever
      sees files a tracked tool call actually wrote. Best-effort: a failed
      scan (missing/inaccessible directory) just leaves the list as-is. */
  refreshArtifactsFromDisk: () => Promise<void>;
  /** Optionally pass an already-fetched provider list (e.g. from
      `loadSession`, which needs one anyway) to skip a redundant
      `listProviders` round-trip. */
  refreshProvider: (providers?: ProviderView[]) => Promise<void>;
  branch: (uiIndex: number) => Promise<void>;
  regenerate: (assistantIndex: number) => Promise<void>;
  addPastedText: (text: string, label?: string) => void;
  removeAttachment: (id: string) => void;
  addPendingImage: (mime: string, dataUrl: string) => void;
  removePendingImage: (id: string) => void;
  exportSession: (upToIndex?: number) => Promise<void>;
  newSession: (cwd?: string) => Promise<void>;
  ensureSession: () => Promise<string>;
  /** `providerId`/`modelId` come from the session's `SessionSummary` (sidebar
      list metadata) — pass them when resuming a session from the sidebar so
      Kitty can restore the provider it was last used with (see
      `sessionConcluded` above). Omit for a same-session reload/handoff,
      where the stored values (if any) are reused automatically. */
  loadSession: (
    sessionId: string,
    cwd: string,
    title?: string,
    providerId?: string,
    modelId?: string
  ) => Promise<void>;
  reloadCurrent: () => Promise<void>;
  send: (text: string) => Promise<void>;
  /** Invoke a recipe: attaches its resolved instructions to the current
      message (or a lazily-created one, via `ensureSession()`) rather than
      forking a new session — the recipe augments whatever conversation is
      already open, and the model decides whether prior history is relevant.
      No-ops while `busy`, same as `send()`/`regenerate()`. */
  sendWithRecipe: (recipe: Recipe, primaryText: string) => Promise<void>;
  cancel: () => Promise<void>;
  /** Hard-reset a stuck turn the user chose to force-stop (Round-5): clears
      `busy`, ends the in-flight message, and abandons the turn so late events
      are ignored. Only meaningful once `stopPhase === 'forceable'`. */
  forceStop: () => void;
  /** Clear the runaway-repetition suspicion flag (see `loopSuspected` below) —
      called when the user dismisses the suggestion, or when a fresh turn
      starts. Does not itself cancel anything; only `cancel()`/`forceStop()`
      do that, and only when the user actually chooses to. */
  dismissLoopWarning: () => void;
  respondApproval: (toolCallId: string, optionId: string | null) => Promise<void>;
  setMode: (modeId: string) => Promise<void>;
  /** Flip the session's chat/agentic mode (Round-4). Persists the override and
      handles the mid-conversation safety/attachment concerns described on
      `send()`'s strip-reasoning STOPGAP neighbor, `bindEvents`' approval
      handler, and `addDroppedPaths` below. */
  setModeOverride: (mode: 'chat' | 'agentic' | null) => Promise<void>;
  /** Set the active session's reasoning effort (Round-7) — live, no goosed
      restart. No-op if there's no active session or effort control isn't
      available for the active model. */
  setThinkingEffort: (value: string) => Promise<void>;
  addDroppedPaths: (paths: string[]) => Promise<void>;
  removeDroppedPath: (path: string) => void;
  setWorkingDir: (folder: string) => Promise<void>;
  adoptSession: (info: {
    session_id: string;
    cwd: string;
    current_mode: string;
    available_modes: ModeInfo[];
    /** Present when handed off mid-turn (Expand while streaming) — the
        overlay's own live render state at the moment of handoff, applied
        after the replay since `session/load` doesn't reliably include an
        in-progress turn's not-yet-committed partial content. */
    messages?: Message[];
    artifacts?: Artifact[];
  }) => Promise<void>;
  /** Hand the current session off to the main window (the overlay's "Expand"
      button). Resets local session state afterward so reopening the overlay lands on
      a blank composer instead of the just-expanded conversation (Round-4
      item 7) — no new goosed session is created here; `ensureSession()`
      lazily makes one on the overlay's next send(). */
  handOffToMain: () => Promise<void>;
  /** Manual re-check of the active provider (the "can't reach" banner's Retry
      button) — reuses the same backend health-check the provider-switch gate
      uses, rather than any background polling. Clears `providerOffline` on
      success. */
  retryConnection: () => Promise<void>;
}

/** Effective chat/agentic mode for the current session: an explicit override
    wins, otherwise agentic (Round-7: providers no longer carry a `tools_enabled`
    default — the per-session toggle is the only mode selector now). Exported
    plain selector — usable as `useChatStore(isChatMode)` in components or
    `isChatMode(get())` inside store actions (Round-4). */
// `null` (no explicit per-session override) defaults to chat mode (owner
// ask) — a new session starts as a reading-friendly thought partner unless
// the user explicitly flips it to agentic via ModeToggle.
export const isChatMode = (s: ChatState): boolean => (s.modeOverride ?? 'chat') === 'chat';

let bound = false;
// Timestamp of the most recent send() call, for the per-response duration
// metric (Round-3 item 2) — module-level like `bound`/`msgSeq` below, since
// there's only ever one in-flight prompt per active session.
let lastSentAt: number | null = null;
// Provider/model actually active at the moment send() fired (Round-4 info
// button) — captured here rather than read from live store state in
// onComplete, since the user can switch providers while a response is still
// streaming; that would otherwise attribute the response to the wrong model.
let lastSentProvider: string | null = null;
let lastSentModel: string | null = null;
// Grace-period timer between "Stop" and offering "Force Stop" (Round-5).
// Module-level like the fields above — one in-flight turn per active session.
const STOP_GRACE_MS = 12_000;
let stopGraceTimer: ReturnType<typeof setTimeout> | null = null;
const clearStopGrace = () => {
  if (stopGraceTimer) {
    clearTimeout(stopGraceTimer);
    stopGraceTimer = null;
  }
};
let msgSeq = 0;
const newId = () => `m${Date.now()}_${++msgSeq}`;

// Live counts backing the chat-mode tool-loop guard (see `countToolCall`
// in ./chat/loopGuards) — module-level like `stopGraceTimer`, reset at the
// start of every fresh turn in `send()`.
let toolLoopCounts: ToolCallCounts = new Map();

// Synchronous in-flight guard for turn submission (Round-WS8): `busy` is
// committed only after several awaits (ensureSession, image reads, …), so a
// rapid double-submit (double-Enter) could both pass the `!busy` gate and
// enqueue two prompts. This flag is set before ANY await and cleared in a
// `finally`, so duplicates are impossible even within the same tick. Shared
// by `send`, `regenerate` and `sendWithRecipe` (a recipe invocation holds it
// for its whole prepare-then-send sequence, so a concurrent plain send can't
// slip in between).
let sendInFlight = false;

// Dedupes concurrent session-creation requests (Round-7): `newSession()`
// clears the UI optimistically before awaiting `ipc.newSession`, so a
// concurrent `send()` (user types+sends before that await resolves) sees
// `sessionId === null` and calls `ensureSession()` → `newSession()` again.
// Without this, that would fire a second real `ipc.newSession` call and
// orphan one of the two goosed sessions. Module-level like the fields above.
let pendingNewSession: Promise<SessionInfo> | null = null;
function getOrCreateSession(cwd?: string, mode?: string | null): Promise<SessionInfo> {
  if (!pendingNewSession) {
    pendingNewSession = ipc.newSession(cwd, mode).finally(() => {
      pendingNewSession = null;
    });
  }
  return pendingNewSession;
}

// Set whenever `onSessionDeleted` tears down *this window's own* active
// session (see that handler below). goosed briefly races on its own side
// right after `session/delete` — the very next `session/new` on the same
// connection can come back "resource not found" even though the new
// session has nothing to do with the deleted one. Real, observed bug:
// Delete-the-chat-you're-in followed by New Chat surfaced that transient
// error to the user instead of just working on a silent retry.
let recentOwnSessionDeleteAt = 0;
const OWN_DELETE_RETRY_WINDOW_MS = 3000;

export const useChatStore = create<ChatState>((set, get) => {
  // Force approve-mode whenever the effective mode is chat, so `auto` can
  // never silently execute a tool call unseen (Round-4 tool-safety fix for the
  // Phase-9 latent bug: goosed always has tools live even in chat-only mode).
  // In approve mode every tool call surfaces as a permission request that
  // `bindEvents`' handler then decides (Round-5: allow, but scoped to the chat
  // folder — see there). Called after every session bootstrap point
  // (new/load/adopt) and on flip-to-chat; best-effort.
  const ensureSafeApprovalMode = async () => {
    const sid = get().sessionId;
    if (!sid || !isChatMode(get()) || get().mode === 'approve') return;
    set({ mode: 'approve' });
    try {
      await ipc.setMode(sid, 'approve');
    } catch {
      /* best-effort */
    }
  };

  // Chat-mode file handling (Phase 9, reused by both `addDroppedPaths` and a
  // mid-conversation agentic→chat flip in `setModeOverride` below): inline a
  // *user-attached* file's content rather than sending a path, so the model
  // sees the content directly without needing a filesystem tool to open a path
  // outside the chat folder. (Distinct from the model's own scoped tool use,
  // which Round-5 now permits inside the chat folder.)
  //
  // Real, observed bug this fixes: images used to fall into the same "binary,
  // can't inline as text" bucket as any other non-UTF8 file, landing as a
  // vague "[Attached file ... contents not inlined]" placeholder with no
  // actual image data and no path either. The model, unable to see the image
  // but told one exists, would try a filesystem tool to go find it — which
  // `decideChatApproval` always declines (the file is outside the chat's own
  // folder), surfacing as "the file is outside its working directory." Images
  // need no filesystem access at all: they go through the exact same native
  // ACP image-content-block path already used for agentic-mode drops and
  // clipboard-pasted images (`pendingImages`/`addPendingImage`), just reached
  // from chat mode's attach flow too now.
  //
  // Genuinely non-inlinable non-image binaries (PDF, DOCX, …) can't be turned
  // into real text content here (that would need real text extraction, out
  // of scope for this fix) — but a real chat-capable model may well have its
  // own document-reading tool for exactly this file type (confirmed via a
  // real captured thought trace: the model correctly identified it had a
  // `.docx`-capable tool available, but the file wasn't on disk anywhere it
  // could reach). So instead of just telling the model to give up, the file
  // is copied into the session's own working directory — the one folder
  // `decideChatApproval` already permits chat-mode tool access to — so the
  // model's own tool can genuinely open it there.
  const addArtifact = (artifact: Artifact) =>
    set((s) => ({
      artifacts: s.artifacts.some((a) => a.path === artifact.path)
        ? s.artifacts
        : [...s.artifacts, artifact],
    }));

  const inlineFileAsAttachment = async (f: PathInfo) => {
    if (f.is_dir) return;
    if (isImageFileName(f.name)) {
      try {
        const file = await ipc.readFileAny(f.path);
        get().addPendingImage(file.mime ?? 'image/png', file.content);
        addArtifact(userFileArtifact(f.path, f.name));
      } catch (e) {
        set({ error: String(e) });
      }
      return;
    }
    try {
      const file = await ipc.readFileAny(f.path);
      if (file.kind === 'text') {
        get().addPastedText(file.content, f.name);
        addArtifact(userFileArtifact(f.path, f.name));
        return;
      }
      // Ensure a real session (and therefore a real working directory)
      // exists before copying into it — a file can be attached before the
      // first message of a brand-new chat is ever sent.
      await get().ensureSession();
      const cwd = get().cwd;
      try {
        if (!cwd) throw new Error('no working directory yet');
        const copiedName = await ipc.copyFileIntoChatFolder(f.path, cwd);
        get().addPastedText(
          `[Attached file "${copiedName}" (${file.mime ?? 'binary'}) has been saved to your ` +
            `working directory as "${copiedName}" — its contents couldn't be extracted as plain ` +
            `text, so use your own file tools to open "${copiedName}" directly if you need to see ` +
            `inside it.]`,
          copiedName
        );
        addArtifact(userFileArtifact(`${cwd.replace(/[\\/]+$/, '')}/${copiedName}`, copiedName));
      } catch {
        // Best-effort fallback: if the copy itself fails (disk full,
        // permissions, etc.), don't leave the model to guess at a path that
        // was never going to work anyway.
        get().addPastedText(
          `[Attached file "${f.name}" (${file.mime ?? 'binary'}) — its contents couldn't be ` +
            `extracted as text and the file itself couldn't be made available to you either. You ` +
            `have no file-system access to it in this chat — don't attempt to open, read, or ` +
            `locate it with a tool. Just let the user know you can't view this file.]`,
          f.name
        );
      }
    } catch (e) {
      set({ error: String(e) });
    }
  };

  // --- Streaming-delta coalescing (Round-5 Batch 5) -------------------------
  // A fast provider emits an `agent_message_chunk` per token; applying each one
  // as its own `set()` re-renders the (growing) markdown message on every
  // token, saturating the webview's single JS thread so even a click — e.g.
  // Expand — isn't handled until the stream drains (diagnosed live: a 1500-
  // delta burst delayed a window op ~3s vs ~4ms idle). So buffer text/reasoning
  // deltas and apply them in one `set()` per animation frame. Ordering with
  // non-delta events (tool cards, completion) is preserved by flushing
  // synchronously at the start of those handlers; session resets discard the
  // buffer so a stale turn's tail never leaks into a new one.
  let pendingText = '';
  let pendingReasoning = '';
  let flushHandle: ReturnType<typeof requestAnimationFrame> | null = null;
  // See `splitLeakedThinkTag` — once a leaked `</think>` marker has been
  // found and corrected for the message currently streaming, stop
  // re-checking, so a later, legitimate mention of that same literal
  // substring in the real answer is never mistaken for another leak.
  let thinkLeakResolved = false;

  const resolveThinkLeak = (
    text: string,
    reasoning: string
  ): { text: string; reasoning: string } => {
    if (thinkLeakResolved) return { text, reasoning };
    const split = splitLeakedThinkTag(text);
    if (!split) return { text, reasoning };
    thinkLeakResolved = true;
    return {
      text: split.text,
      reasoning: reasoning ? `${reasoning}\n${split.reasoning}` : split.reasoning,
    };
  };

  // Some OpenAI-compatible endpoints (observed with a custom local server)
  // re-emit the *entire* completion as one final `agent_message_chunk` after
  // having already streamed it token-by-token, instead of a genuine
  // incremental delta — goosed forwards whatever chunk it gets, so without
  // this guard the message ends up with its own full text duplicated back to
  // back. A real incremental chunk is never byte-identical to everything
  // already accumulated, so this only ever catches that echo (guarded by a
  // length floor so a short, coincidentally-repeated delta can't trip it).
  const ECHO_GUARD_MIN_LEN = 24;
  const dropEchoedChunk = (soFar: string, incoming: string) =>
    incoming.length >= ECHO_GUARD_MIN_LEN && incoming === soFar ? '' : incoming;

  const flushDeltas = () => {
    if (flushHandle != null) {
      cancelAnimationFrame(flushHandle);
      flushHandle = null;
    }
    if (!pendingText && !pendingReasoning) return;
    const t = pendingText;
    const r = pendingReasoning;
    pendingText = '';
    pendingReasoning = '';
    set((s) => {
      const msgs = s.messages.slice();
      const last = msgs[msgs.length - 1];
      if (
        last &&
        last.role === 'assistant' &&
        (last.open || isStragglerAssistantMessage(last, s.busy))
      ) {
        const fixed = resolveThinkLeak(
          last.text + dropEchoedChunk(last.text, t),
          last.reasoning + dropEchoedChunk(last.reasoning, r)
        );
        msgs[msgs.length - 1] = { ...last, ...fixed };
        return { messages: msgs };
      }
      const fixed = resolveThinkLeak(t, r);
      const closed = closeOpen(msgs);
      closed.push({
        id: newId(),
        role: 'assistant',
        ...fixed,
        toolCalls: [],
        streaming: true,
        open: true,
      });
      return { messages: closed };
    });

    // Runaway-generation guard (see `hasRepetitionLoop`'s doc comment) —
    // checked on every flush, not every raw token, so it stays cheap. Gated on
    // `!s.replaying`, not just `s.busy`: `loadSession` sets `busy: true` for
    // the whole `session/load` replay too (it's the same event pipeline as a
    // live turn), so a long-but-legitimate historical message — reconstructed
    // in a handful of rapid flush bursts rather than gradually over real
    // time — could otherwise trip this heuristic purely from replaying old,
    // already-fine history (confirmed real report: loading a long past chat
    // surfaced this suggestion with nothing actually running). This only
    // *suggests* cancelling (via `loopSuspected`) — it never cancels on its
    // own; the model may yet recover, and the choice is the user's.
    //
    // Also gated on `!s.activeRecipeTurn`: a recipe (e.g. the debate
    // moderator, which deliberately produces structurally-repetitive
    // "FOR — Round N" / "AGAINST — Round N" output) can legitimately look
    // like a repetition loop to this heuristic. Recipe turns get their own,
    // stricter enforcement instead — see the reasoning-token hard cap below —
    // so suppressing this suggestion here doesn't leave them unbounded.
    const s = get();
    if (!s.loopSuspected && !s.activeRecipeTurn) {
      const last = s.messages[s.messages.length - 1];
      if (
        s.busy &&
        !s.replaying &&
        last?.role === 'assistant' &&
        last.open &&
        hasRepetitionLoop(last.reasoning + '\n' + last.text)
      ) {
        set({ loopSuspected: true });
      }
    }

    // Recipe reasoning hard cap — the safety net the loop-detection suppression
    // above needs: unlike general chat (where a suggestion is enough and the
    // user decides), a recipe's `max_reasoning_tokens` is an explicit,
    // enforced limit, so exceeding it auto-cancels the turn rather than just
    // suggesting it. Approximated via character count (~4 chars/token for
    // English text) since ACP exposes no numeric reasoning-token config to
    // check against — only effort levels (confirmed via docs/acp-protocol.md;
    // see `Recipe.max_reasoning_tokens`'s doc comment). `activeRecipeTurn` is
    // cleared as part of the same `set()` that triggers the cancel, so this
    // can only fire once per turn — no risk of calling `cancel()` repeatedly
    // on every subsequent flush.
    if (s.activeRecipeTurn && s.busy && !s.replaying) {
      const last = s.messages[s.messages.length - 1];
      if (
        last?.role === 'assistant' &&
        last.open &&
        exceedsReasoningCap(last.reasoning.length, s.activeRecipeTurn.maxReasoningTokens)
      ) {
        set({
          activeRecipeTurn: null,
          warning: `This recipe's response hit its reasoning cap (${s.activeRecipeTurn.maxReasoningTokens} tokens) — stopping it and asking for a direct answer.`,
          // There's no ACP way to redirect a generation already in flight
          // straight to its answer, only to cancel the whole turn — so once
          // this cancellation actually completes (`onComplete`/`onChatError`),
          // a forced follow-up turn asks for one instead, rather than leaving
          // the user with nothing.
          pendingForcedAnswer: s.sessionId,
        });
        void get().cancel();
      }
    }
  };

  const bufferDelta = (field: 'text' | 'reasoning', text: string) => {
    if (field === 'text') pendingText += text;
    else pendingReasoning += text;
    if (flushHandle == null) flushHandle = requestAnimationFrame(flushDeltas);
  };

  const discardDeltas = () => {
    if (flushHandle != null) {
      cancelAnimationFrame(flushHandle);
      flushHandle = null;
    }
    pendingText = '';
    pendingReasoning = '';
    thinkLeakResolved = false;
  };

  // The actual turn-submission body, shared by `send()` and `sendWithRecipe()`.
  // Callers MUST have already acquired the `sendInFlight` guard — this core
  // deliberately does not re-check it (a recipe holds the guard across its
  // whole prepare-then-send sequence and then delegates here). Returns whether
  // the prompt was actually handed to the backend (the caller's guard-abort
  // rollback depends on it: if we aborted before submission, side effects like
  // a recipe's agentic flip should be undone; if we submitted, don't).
  const doSend = async (text: string): Promise<boolean> => {
    const trimmed = text;
    const attachments = get().attachments;
    const pendingImages = get().pendingImages;
    let submitted = false;
    try {
      const firstMessage = get().messages.length === 0;
      const chatOnly = isChatMode(get());
      let sessionId: string;
      try {
        sessionId = await get().ensureSession();
      } catch (e) {
        // Real, observed bug: an uncaught failure here (e.g. goosed down)
        // used to throw out of send() with no user-facing error and, on a
        // concurrent/overlapping call, could leave `busy` stuck true —
        // reset it explicitly and surface the error instead.
        set({ busy: false, error: String(e) });
        return false;
      }
      const files = get().droppedFiles;

      // Agentic-mode images go as native ACP image content blocks instead of a
      // bare filesystem path reference (Round-3 item 17 — fixes untrusted/remote
      // providers failing with "file not found": the model no longer has to
      // correctly invoke a file tool on an exact path just to see the picture).
      // `files` (droppedFiles) is only ever populated in agentic mode —
      // chat-only mode's `addDroppedPaths` routes every drop straight through
      // `inlineFileAsAttachment` instead (which now sends chat-mode image
      // drops through the exact same native-image-block mechanism, just via
      // `pendingImages` below rather than this `droppedFiles`-derived list).
      const imageFiles = files.filter((f) => !f.is_dir && isImageFileName(f.name));
      const otherFiles = files.filter((f) => !imageFiles.includes(f));
      for (const f of [...otherFiles, ...imageFiles]) addArtifact(userFileArtifact(f.path, f.name));
      let images: { mime: string; data_url: string }[] | undefined;
      if (imageFiles.length) {
        const results = await Promise.all(
          imageFiles.map(async (f) => {
            try {
              const file = await ipc.readFileAny(f.path);
              return { mime: file.mime ?? 'image/png', data_url: file.content };
            } catch (e) {
              set({ error: String(e) });
              return null;
            }
          })
        );
        images = results.filter((r): r is { mime: string; data_url: string } => r !== null);
      }
      // Clipboard-attached images (Round-4) go through regardless of mode —
      // unlike the droppedFiles-based extraction above, which is specifically
      // about file *drops* and stays agentic-only.
      if (pendingImages.length) {
        images = [
          ...(images ?? []),
          ...pendingImages.map((p) => ({ mime: p.mime, data_url: p.data_url })),
        ];
      }

      let promptText = trimmed;
      if (chatOnly && attachments.length) {
        // Inline document content directly (no filesystem tool in chat-only mode).
        const docs = attachments.map((a) => `--- ${a.label} ---\n${a.content}`).join('\n\n');
        promptText = `${docs}\n\n${trimmed}`.trim();
      } else if (otherFiles.length) {
        // Agentic: hand non-image paths to the filesystem tools (CLAUDE.md §5).
        const block =
          'Files provided by the user:\n' + otherFiles.map((f) => `- ${f.path}`).join('\n');
        promptText = `${block}\n\n${trimmed}`;
      }

      // STOPGAP client-side workaround (see buildStrippedTranscript's doc comment):
      // only engages once some prior assistant turn actually reasoned — a turn
      // with nothing to strip shouldn't pay for a session swap. Prior turns come
      // from local render state, since goosed's own history is exactly what we're
      // bypassing here.
      const priorMessages = get().messages;
      const stripReasoningNow =
        chatOnly &&
        get().stripReasoning &&
        priorMessages.some((m) => m.role === 'assistant' && m.reasoning.trim().length > 0);
      if (stripReasoningNow) {
        promptText = `${buildStrippedTranscript(priorMessages)}\n\nUser: ${promptText}`;
      }
      // Custom/default system prompt (Round-6 Feature 2), first turn of a
      // session only — set server-side via BigTiny's real `persona_override`
      // session-metadata field (same mechanism recipes use), rendered as a
      // proper `role: "system"` message by ContextBuilder::build_messages.
      // Previously this prepended a literal `<system>...</system>` block onto
      // the outgoing *user* message text — a leftover from the pre-BigTiny
      // Goose/ACP backend, which had no system-prompt field of its own.
      // Embedding fake role markup inside a user turn is exactly the kind of
      // malformed input a model whose chat template expects strict role/tag
      // structure can derail on (observed: a llama-server-hosted model
      // hallucinating an unrelated persona and looping). Best-effort — a
      // failure here (e.g. BigTiny transiently unreachable) shouldn't block
      // the turn; it just falls back to BigTiny's generic built-in persona.
      // `firstMessage` was captured before the stripReasoning session-swap
      // logic above, so a mid-conversation swap onto a fresh goosed session
      // correctly does NOT get persona_override set again — from the user's
      // perspective it's a continuation, not a new conversation.
      if (firstMessage) {
        const resolvedPrompt = get().systemPrompt ?? defaultSystemPrompt(chatOnly);
        try {
          await ipc.setSessionPersonaOverride(sessionId, resolvedPrompt);
        } catch (e) {
          console.warn('setSessionPersonaOverride failed, continuing with default persona', e);
        }
      }
      // Recipe invocation (`sendWithRecipe`) — unlike the system-prompt
      // persona above, this applies on ANY turn, not just a session's first
      // message: a recipe can be invoked at any point in a conversation,
      // attaching to whatever's already open rather than requiring a fresh
      // session. The recipe wrapper is still a text preamble on `promptText`
      // itself (not `persona_override`) because, unlike the session-level
      // persona, it's meant to be a one-off, mandatory instruction for THIS
      // turn only, not a persisted system message every future turn resends.
      const recipeCard = get().pendingRecipeCard;
      if (recipeCard) {
        promptText =
          `<recipe title="${recipeCard.title}">\n${recipeCard.instructions}\n</recipe>\n\n` +
          `Run the recipe above now — it is mandatory for this message. You may use the ` +
          `conversation so far if it's relevant, but you are not required to.\n\n${promptText}`;
      }
      // Captured before the consume-once clear below, so the busy:true set()
      // further down can derive activeRecipeTurn from it.
      const recipeMaxReasoningTokens = recipeCard?.maxReasoningTokens ?? null;
      set({ pendingRecipeCard: null }); // consume-once, unconditionally
      const cwd = get().cwd ?? undefined;

      // Snapshot what's attached to this turn before the set() below clears
      // droppedFiles/attachments/pendingImages from composer state — otherwise
      // there'd be no record of it on the sent message at all (Round-7 fix).
      const attachedFiles: { name: string; kind: 'file' | 'document' | 'image' }[] = [
        ...otherFiles.map((f) => ({ name: f.name, kind: 'file' as const })),
        ...imageFiles.map((f) => ({ name: f.name, kind: 'image' as const })),
        ...attachments.map((a) => ({ name: a.label, kind: 'document' as const })),
        ...pendingImages.map((_p, i) => ({
          name: pendingImages.length > 1 ? `Clipboard image ${i + 1}` : 'Clipboard image',
          kind: 'image' as const,
        })),
      ];

      const userMsg: Message = {
        id: newId(),
        role: 'user',
        text:
          trimmed ||
          (attachments.length
            ? `(${attachments.length} document(s))`
            : pendingImages.length
              ? `(${pendingImages.length} image(s))`
              : ''),
        reasoning: '',
        toolCalls: [],
        streaming: false,
        open: false,
        attachedFiles: attachedFiles.length ? attachedFiles : undefined,
      };
      // Fresh turn: clear any leftover stop/abandon state so its events flow
      // and a prior force-stop on this session no longer suppresses them.
      // Also reset the tool-loop guard — a call repeating across different
      // turns is normal, not a loop.
      clearStopGrace();
      discardDeltas();
      toolLoopCounts = new Map();
      set((s) => ({
        messages: [...s.messages, userMsg],
        droppedFiles: [],
        attachments: [],
        pendingImages: [],
        busy: true,
        error: null,
        errorType: null,
        // Optimistic clear: a stale "can't reach the provider" banner from a
        // previous failure should not persist across a fresh send attempt —
        // this is the actual fix for the "stuck until restart" bug, since
        // this was the only field of its kind with no self-clearing path at
        // all. If this attempt also fails, onChatError/emit_health_from_send_result
        // will set it again.
        providerOffline: false,
        stopPhase: null,
        abandonedSession: null,
        loopSuspected: false,
        activeRecipeTurn:
          recipeMaxReasoningTokens != null
            ? { maxReasoningTokens: recipeMaxReasoningTokens }
            : null,
      }));
      lastSentAt = performance.now();
      lastSentProvider = get().providerName;
      lastSentModel = get().model;
      if (stripReasoningNow) {
        // Swap to a brand-new goosed session carrying only the reconstructed,
        // reasoning-free transcript — never the old session (which still has
        // goosed's own unstripped history). `sessionId` (and `mode`) MUST be
        // updated before `sendPrompt` fires, not after: `bindEvents()`'s stream
        // handlers all gate on `forActive(sid)` (`get().sessionId === sid`), so
        // deferring the swap would silently drop every event for this turn.
        const oldSessionId = sessionId;
        const oldModeOverride = get().modeOverride;
        const info = await ipc.newSession(cwd, oldModeOverride ?? 'chat');
        // Best-effort — see the identical bindWindowSession call above.
        void ipc.bindWindowSession(info.session_id).catch(() => {});
        set({
          sessionId: info.session_id,
          mode: info.current_mode,
          availableModes: info.available_modes,
        });
        // Carry the mode override across to the new session id (it's the same
        // conversation from the user's perspective) and force a safe approval
        // mode on it, same as any other freshly-established chat-mode session.
        if (oldModeOverride) {
          void ipc.setSessionMode(info.session_id, oldModeOverride).catch((e) => {
            // Not fatal, but the swapped-to session loses its carried-forward
            // mode override and falls back to default mode detection.
            console.warn('setSessionMode failed on stripReasoning swap', e);
          });
        }
        await ensureSafeApprovalMode();
        submitted = true;
        try {
          await ipc.sendPrompt(info.session_id, promptText, images);
        } catch (e) {
          // The new session never got a real turn — drop it and restore the
          // old (still fully intact) session rather than losing the thread.
          // No `cwd` here: the working directory is shared with the session
          // being restored, so skip delete_session's directory cleanup.
          void ipc.deleteSession(info.session_id).catch(() => {});
          void ipc.setSessionMode(info.session_id, null).catch(() => {});
          set({ sessionId: oldSessionId, modeOverride: oldModeOverride });
          throw e;
        }
        // Success: best-effort cleanup of the now-superseded old session and
        // its mode-override entry — a failure here shouldn't surface as an
        // error for a turn that actually succeeded. Same no-`cwd` reasoning as
        // above (shared working dir).
        void ipc.deleteSession(oldSessionId).catch(() => {});
        void ipc.setSessionMode(oldSessionId, null).catch(() => {});
        // Chat-only sessions stay in the overlay — no auto-promote to the full
        // window (owner decision: the overlay is a fully capable chat surface
        // on its own; "Expand" remains available for anyone who wants the
        // bigger window, but it's never forced).
      } else {
        submitted = true;
        await ipc.sendPrompt(sessionId, promptText, images);
      }
      return submitted;
    } catch (e) {
      set({ busy: false, error: String(e) });
      return false;
    }
  };

  return {
    sessionId: null,
    cwd: null,
    sessionEpoch: 0,
    backgroundSession: null,
    backgroundTurnToast: null,
    chatDir: null,
    title: null,
    mode: null,
    availableModes: [],
    thinkingEffort: null,
    creatingSession: false,
    messages: [],
    artifacts: [],
    droppedFiles: [],
    attachments: [],
    pendingImages: [],
    pendingAttachments: [],
    pendingApprovals: [],
    busy: false,
    sessionProviderId: null,
    sessionModelId: null,
    sessionConcluded: false,
    replaying: false,
    error: null,
    errorType: null,
    providerTier: null,
    providerHost: null,
    providerOffline: false,
    checkingConnection: false,
    isTrusted: false,
    model: null,
    providerName: null,
    stripReasoning: false,
    systemPrompt: null,
    modeOverride: null,
    savedApprovalMode: null,
    warning: null,
    compactionNotice: null,
    stopPhase: null,
    abandonedSession: null,
    loopSuspected: false,
    recipes: [],
    pendingRecipeCard: null,
    activeRecipeTurn: null,
    pendingForcedAnswer: null,

    dismissWarning: () => set({ warning: null }),
    dismissCompactionNotice: () => set({ compactionNotice: null }),
    dismissBackgroundToast: () => set({ backgroundTurnToast: null }),

    compact: async () => {
      const sid = get().sessionId;
      if (!sid) return;
      try {
        const r = await ipc.compactSession(sid);
        set({
          compactionNotice: r.compacted
            ? `Context manually compacted: ${r.messages_compacted ?? 0} older turns folded (${r.tokens_before ?? 0} → ${r.tokens_after ?? 0} tokens).`
            : 'Nothing old enough to compact yet.',
        });
      } catch {
        set({ compactionNotice: 'Compact failed — check the backend is healthy.' });
      }
    },

    pruneMissingArtifacts: async () => {
      const artifacts = get().artifacts;
      if (artifacts.length === 0) return;
      try {
        const infos = await ipc.inspectPaths(artifacts.map((a) => a.path));
        const missing = new Set(infos.filter((i) => !i.exists).map((i) => i.path));
        if (missing.size === 0) return;
        set((s) => ({ artifacts: s.artifacts.filter((a) => !missing.has(a.path)) }));
      } catch {
        // best-effort — try again on the next call
      }
    },

    refreshArtifactsFromDisk: async () => {
      const cwd = get().cwd;
      if (!cwd) return;
      try {
        const entries = await ipc.listDirectory(cwd);
        // Compare on a normalized (forward-slash, lowercased) form — Windows
        // paths are case-insensitive and tool-derived artifact paths can mix
        // separators (see absoluteArtifactPath in messageUtils.ts), so raw
        // string equality would under-dedupe against disk-scanned entries.
        const normalize = (p: string) => p.replace(/\\/g, '/').toLowerCase();
        set((s) => {
          const known = new Set(s.artifacts.map((a) => normalize(a.path)));
          const additions: Artifact[] = entries
            .filter((e) => !known.has(normalize(e.path)))
            .map((e) => ({ path: e.path, name: e.name, tool: 'disk', source: 'disk' as const }));
          if (additions.length === 0) return s;
          return { artifacts: [...s.artifacts, ...additions] };
        });
      } catch {
        // best-effort — try again on the next call (e.g. cwd not yet created)
      }
    },

    refreshProvider: async (prefetched?: ProviderView[]) => {
      try {
        const providers = prefetched ?? (await ipc.listProviders());
        const active = providers.find((p) => p.active);
        set({
          providerTier: active ? active.network_tier : null,
          providerHost: active ? new URL(active.base_url).host : null,
          isTrusted: active ? active.is_trusted : false,
          model: active?.models[0] ?? null,
          providerName: active ? active.name || active.provider_type : null,
          stripReasoning: active ? active.strip_reasoning : false,
          systemPrompt: active ? active.system_prompt : null,
        });
        if (active) {
          const cur = get();
          if (cur.sessionId === null && cur.mode === null) {
            // No session has ever existed in this window yet — seed from the
            // last-known values for this provider so the dropdowns don't start
            // blank (see the cache's doc comment above).
            const cached = readCachedModeInfo(active.id);
            if (cached) {
              set({
                mode: cached.mode,
                availableModes: cached.availableModes,
                thinkingEffort: cached.thinkingEffort,
              });
            }
          } else if (cur.sessionId !== null && cur.mode !== null) {
            // A real session's values are live — refresh the cache for next time.
            writeCachedModeInfo(active.id, {
              mode: cur.mode,
              availableModes: cur.availableModes,
              thinkingEffort: cur.thinkingEffort,
            });
          }
        }
      } catch {
        set({
          providerTier: null,
          providerHost: null,
          // A stale `true` here would mislead `addDroppedPaths` into skipping
          // the untrusted-provider file warning after a failed refresh — reset
          // it alongside the rest of the derived provider state.
          isTrusted: false,
          model: null,
          providerName: null,
          stripReasoning: false,
          systemPrompt: null,
        });
      }
    },

    adoptSession: async (info) => {
      // Replay the handed-off conversation (Expand / auto-promote) so the full
      // window shows it — previously this only set the session id, leaving the
      // window blank until the next streamed token. loadSession does the replay
      // plus mode/provider/approval setup; goosed is the source of truth.
      await get().loadSession(info.session_id, info.cwd);
      // If handed off mid-turn, overwrite whatever the replay produced with
      // the overlay's own accurate render-state snapshot — session/load's
      // replay doesn't reliably include an in-progress turn's not-yet-
      // committed partial content (confirmed real report: some of the
      // response being generated didn't transfer on Expand). The snapshot's
      // last message already carries the correct `open`/`streaming` state, so
      // further live deltas for this session (still arriving, since goosed
      // keeps generating) append to it normally instead of spawning a
      // duplicate.
      if (info.messages) {
        set({ messages: info.messages, artifacts: info.artifacts ?? [] });
      }
    },

    handOffToMain: async () => {
      const s = get();
      // Feature 5: Expand always opens a brand-new chat window (never
      // re-targets an already-open one) — the handoff snapshot travels as a
      // direct argument to that new window's creation, keyed by its own
      // label server-side, rather than through the older global
      // `active_session` slot (`setActiveSession`/`session://active`), which
      // remains in place only for the unrelated provider context-handoff
      // gate and would otherwise race every open chat window into adopting
      // the same handoff.
      const handoff = s.sessionId
        ? {
            session_id: s.sessionId,
            cwd: s.cwd ?? '',
            current_mode: s.mode ?? 'auto',
            available_modes: s.availableModes,
            thinking_effort: s.thinkingEffort,
            // Snapshot of live render state, for adoptSession() to apply after
            // its replay — see that action's own comment for why this is
            // needed (session/load's replay alone can drop an in-progress
            // turn's partial content).
            messages: s.messages,
            artifacts: s.artifacts,
          }
        : null;
      await ipc.openNewChatWindow(handoff);
      await ipc.hideOverlay();
      // No new goosed session is created here — ensureSession() lazily makes
      // one the next time this (now-blank) overlay actually sends a message.
      clearStopGrace();
      discardDeltas();
      set({
        sessionId: null,
        cwd: null,
        chatDir: null,
        title: null,
        mode: null,
        availableModes: [],
        thinkingEffort: null,
        messages: [],
        artifacts: [],
        droppedFiles: [],
        attachments: [],
        pendingImages: [],
        pendingAttachments: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
        errorType: null,
        providerOffline: false,
        busy: false,
        stopPhase: null,
        abandonedSession: null,
        loopSuspected: false,
        activeRecipeTurn: null,
      });
    },

    retryConnection: async () => {
      set({ checkingConnection: true });
      try {
        await ipc.testActiveProviderConnection();
        set({ providerOffline: false });
      } catch (e) {
        set({ providerOffline: true, error: String(e) });
      } finally {
        set({ checkingConnection: false });
      }
    },

    newSession: async (cwd?: string) => {
      // WS8: if a turn is in flight for the session this window is leaving,
      // keep it running in the background (approved design) — remember it so
      // its completion surfaces as a toast with a "jump back" action, and a
      // subtle "still running" indicator shows meanwhile.
      const leaving = get();
      if (leaving.busy && leaving.sessionId) {
        set({
          backgroundSession: {
            sessionId: leaving.sessionId,
            cwd: leaving.cwd ?? '',
            title: leaving.title,
          },
        });
      }
      // Bump the session epoch up front — any in-flight `loadSession` replay
      // (which captured the previous epoch) will see the mismatch in its
      // finally and skip applying its stale replay state to this new chat.
      const epoch = get().sessionEpoch + 1;
      // Optimistic clear (owner: New Chat should manifest instantly, not only
      // once the ACP round trip(s) finish) — the blank chat shows immediately;
      // `sessionId: null` here is safe against a concurrent send() racing in
      // (it would call ensureSession() → newSession() again, but
      // getOrCreateSession dedupes the actual IPC call below).
      //
      // `mode`/`availableModes`/`thinkingEffort` are deliberately left as-is
      // here (NOT nulled) — a fresh session on the same provider/model will
      // almost always have the same values, so carrying the outgoing
      // session's forward avoids `EffortDropdown`/`ModeBadge` visibly
      // popping in late relative to `ModeToggle`/`ProviderBadge` (which never
      // depended on session data in the first place). `creatingSession` gates
      // interactivity on those two controls instead, so a click during the
      // gap can't act on a session id that doesn't exist yet. The real
      // `session/new` response below is still the sole source of truth and
      // overwrites these the moment it lands.
      clearStopGrace();
      discardDeltas();
      set({
        sessionId: null,
        sessionEpoch: epoch,
        // A fresh blank chat is never "replaying" — leaving the stale value
        // would pin the "Loading conversation…" placeholder on it forever,
        // since the epoch guard now stops the old replay from clearing it.
        replaying: false,
        cwd: null,
        chatDir: null,
        creatingSession: true,
        title: null,
        messages: [],
        artifacts: [],
        droppedFiles: [],
        attachments: [],
        pendingImages: [],
        // pendingAttachments intentionally NOT cleared here: ensureSession()
        // calls newSession() mid-flight while a drop's own addDroppedPaths
        // is still copying a file into the not-yet-created session's folder
        // (see inlineFileAsAttachment's ensureSession() call) — clearing it
        // here flash-hid that in-flight chip. addDroppedPaths's own `finally`
        // already removes exactly the paths it added once that flow settles,
        // so nothing can linger.
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
        errorType: null,
        providerOffline: false,
        busy: false,
        sessionProviderId: null,
        sessionModelId: null,
        sessionConcluded: false,
        stopPhase: null,
        abandonedSession: null,
        loopSuspected: false,
        activeRecipeTurn: null,
      });
      try {
        let info: SessionInfo;
        try {
          info = await getOrCreateSession(cwd, get().modeOverride ?? 'chat');
        } catch (e) {
          // Suppress the transient "resource not found" goosed throws when
          // New Chat lands immediately after deleting the chat that was
          // active — silently retry once rather than showing the user an
          // error for a race that isn't actually their new session's fault.
          if (Date.now() - recentOwnSessionDeleteAt > OWN_DELETE_RETRY_WINDOW_MS) throw e;
          recentOwnSessionDeleteAt = 0;
          info = await getOrCreateSession(cwd, get().modeOverride ?? 'chat');
        }
        recentOwnSessionDeleteAt = 0;
        set({
          sessionId: info.session_id,
          cwd: info.cwd,
          chatDir: info.cwd,
          mode: info.current_mode,
          availableModes: info.available_modes,
          thinkingEffort: info.thinking_effort,
          creatingSession: false,
        });
        // Best-effort: a failure here only means a later notification for
        // this session focuses a generic fallback window instead of this
        // specific one — no data loss, nothing else depends on it.
        void ipc.bindWindowSession(info.session_id).catch(() => {});
        await get().refreshProvider();
        await ensureSafeApprovalMode();
      } catch (e) {
        // Real, observed bug: an uncaught failure here (e.g. goosed briefly
        // down right after a Delete) used to leave `creatingSession: true`
        // forever with no session id — the composer looked alive but every
        // send silently no-opped. Surface the error and drop back to a
        // recoverable (not "stuck loading") state instead.
        set({ creatingSession: false, error: String(e) });
      }
    },

    ensureSession: async () => {
      const current = get().sessionId;
      if (current) return current;
      await get().newSession();
      return get().sessionId!;
    },

    loadSession: async (
      sessionId: string,
      cwd: string,
      title?: string,
      providerId?: string,
      modelId?: string
    ) => {
      // Set the id first so replayed events (which arrive during the call) match.
      // Clear any force-stop abandonment up front — otherwise reloading a
      // previously force-stopped session would drop its replay events.
      clearStopGrace();
      discardDeltas();
      // A reload of the SAME session (reloadCurrent) reuses whatever
      // provider/model this store already has on file for it; a different
      // or brand-new session (handoff/branch/regenerate — always already on
      // the currently-active provider by construction) starts unknown rather
      // than inheriting a stale value from whatever was loaded before it.
      const prior = get();
      const sameSession = prior.sessionId === sessionId;
      const resolvedProviderId = providerId ?? (sameSession ? prior.sessionProviderId : null);
      const resolvedModelId = modelId ?? (sameSession ? prior.sessionModelId : null);
      // WS8: switching to a DIFFERENT session while this window's turn is in
      // flight — keep that turn running in the background and remember it (see
      // `backgroundSession`); its completion later surfaces as a toast here.
      if (prior.busy && prior.sessionId && prior.sessionId !== sessionId) {
        set({
          backgroundSession: {
            sessionId: prior.sessionId,
            cwd: prior.cwd ?? '',
            title: prior.title,
          },
        });
      }
      // If the user is re-opening the very session we backgrounded (e.g. from
      // the completion toast), it's foreground again — drop any pending
      // background indicator/toast for it.
      if (get().backgroundSession?.sessionId === sessionId) {
        set({ backgroundSession: null, backgroundTurnToast: null });
      }
      // Bump the session epoch so an older, still-in-flight loadSession (this
      // window is heavily async) can detect it's stale and skip its finally.
      const epoch = get().sessionEpoch + 1;
      // Best-effort — see the identical bindWindowSession call above.
      void ipc.bindWindowSession(sessionId).catch(() => {});
      set({
        sessionId,
        sessionEpoch: epoch,
        cwd,
        chatDir: cwd,
        title: title ?? null,
        thinkingEffort: null,
        messages: [],
        artifacts: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
        errorType: null,
        providerOffline: false,
        busy: true,
        replaying: true,
        sessionProviderId: resolvedProviderId,
        sessionModelId: resolvedModelId,
        sessionConcluded: false,
        stopPhase: null,
        abandonedSession: null,
        loopSuspected: false,
        activeRecipeTurn: null,
      });
      try {
        // Restore the provider this session was last used with, if it's
        // still around. Under BigTiny provider is PER-SESSION — resolved from
        // the session's metadata at send time — so this stamps only this
        // session (set_session_provider PATCHes its config); it must NOT flip
        // the global active provider, which would cascade into other open
        // windows. If nothing currently configured matches, the profile was
        // deleted since this chat was last used — don't touch anything;
        // instead mark it concluded so the composer blocks new sends while
        // still showing the history below.
        const providers = await ipc.listProviders();
        if (resolvedProviderId && resolvedModelId) {
          const matched = findMatchingProvider(providers, resolvedProviderId, resolvedModelId);
          if (matched) {
            await ipc.setSessionProvider(sessionId, matched.id, resolvedModelId);
          } else {
            set({ sessionConcluded: true });
          }
        }
        const info = await ipc.loadSession(sessionId, cwd);
        set({
          mode: info.current_mode,
          availableModes: info.available_modes,
          thinkingEffort: info.thinking_effort,
        });
        await get().refreshProvider(undefined);
        const override = await ipc.getSessionMode(sessionId).catch(() => null);
        set({ modeOverride: (override as 'chat' | 'agentic' | null) ?? null });
        await ensureSafeApprovalMode();
      } catch (e) {
        set({ error: String(e) });
      } finally {
        // WS8: if the user already moved on to a different session (New Chat
        // or another loadSession) while this replay was in flight, the captured
        // epoch no longer matches — skip applying the replay-completion state
        // entirely, or it would clobber whatever the newer session now holds.
        if (epoch !== get().sessionEpoch) {
          discardDeltas();
        } else {
          // Apply any buffered replay tail before closing the open message — else
          // a pending rAF flush lands on a closed message and spawns a spurious
          // trailing assistant message (Round-5 coalescing interaction).
          flushDeltas();
          // If this session's turn is still genuinely in flight (Expand
          // mid-stream, or just resuming a session another window/process is
          // actively driving), `session/load`'s replay doesn't reliably convey
          // that — check fresh rather than assume idle, so the progress
          // indicator reflects reality instead of looking stalled. Re-open the
          // last message when it's the in-progress assistant turn so further
          // streamed deltas append to it instead of spawning a duplicate bubble.
          const stillBusy = await ipc.isSessionBusy(sessionId).catch(() => false);
          set((s) => {
            const msgs = closeOpen(s.messages);
            if (stillBusy) {
              const last = msgs[msgs.length - 1];
              if (last?.role === 'assistant') msgs[msgs.length - 1] = { ...last, open: true };
            }
            return { busy: stillBusy, replaying: false, messages: msgs };
          });
        }
      }
    },

    cancel: async () => {
      const sid = get().sessionId;
      if (!sid || !get().busy || get().stopPhase) return;
      // Send the cancel notification, but DON'T optimistically release the UI
      // (Round-5). goosed's `session/cancel` is fire-and-forget; if its call to
      // a remote provider is genuinely hung it may never wind down. So we stay
      // "stopping" and give goosed a grace period to actually finish. If a
      // completion/error arrives first, onComplete/onChatError clear this. If
      // the grace elapses, escalate to a user-clickable Force Stop.
      set({ stopPhase: 'stopping' });
      void ipc.cancelPrompt(sid).catch((e) => set({ error: String(e) }));
      clearStopGrace();
      stopGraceTimer = setTimeout(() => {
        stopGraceTimer = null;
        if (get().busy && get().stopPhase === 'stopping') set({ stopPhase: 'forceable' });
      }, STOP_GRACE_MS);
    },

    forceStop: () => {
      clearStopGrace();
      discardDeltas();
      const sid = get().sessionId;
      set((s) => ({
        busy: false,
        stopPhase: null,
        // Abandon this turn so any late stream events for it are dropped by
        // `forActive` until the next send() starts a fresh turn.
        abandonedSession: sid,
        // Clear any stale approval prompts queued for the abandoned turn —
        // leaving them rendered/clickable against a turn that no longer
        // exists (and whose tool call will never resume) is a dead end.
        pendingApprovals: [],
        messages: closeOpen(s.messages),
        warning: 'Stopped. Kitty may still be finishing this turn in the background.',
        loopSuspected: false,
        activeRecipeTurn: null,
        // An explicit Force Stop overrides the reasoning-cap's own automatic
        // cancel-then-ask-for-an-answer flow — no surprise follow-up after
        // the user deliberately kills a turn themselves.
        pendingForcedAnswer: null,
      }));
    },

    dismissLoopWarning: () => set({ loopSuspected: false }),

    reloadCurrent: async () => {
      const { sessionId, cwd, title } = get();
      if (sessionId && cwd) await get().loadSession(sessionId, cwd, title ?? undefined);
    },

    respondApproval: async (toolCallId: string, optionId: string | null) => {
      try {
        await ipc.respondPermission(toolCallId, optionId);
        // Remove the approval from the pending queue only on SUCCESS — dropping
        // it optimistically before the IPC round-trip meant a failure left no
        // way to retry (the prompt vanished with no way back).
        set((s) => ({
          pendingApprovals: s.pendingApprovals.filter((a) => a.tool_call_id !== toolCallId),
        }));
      } catch (e) {
        set({ error: String(e) });
      }
    },

    setMode: async (modeId: string) => {
      const sid = get().sessionId;
      if (!sid) return;
      set({ mode: modeId });
      try {
        await ipc.setMode(sid, modeId);
      } catch (e) {
        set({ error: String(e) });
      }
    },

    setModeOverride: async (mode: 'chat' | 'agentic' | null) => {
      const sessionId = get().sessionId;
      const wasChatMode = isChatMode(get());
      set({ modeOverride: mode });
      const nowChatMode = isChatMode(get());
      if (sessionId)
        void ipc.setSessionMode(sessionId, mode).catch((e) => set({ error: String(e) }));

      if (!wasChatMode && nowChatMode) {
        // agentic → chat: nothing should keep running/waiting on tools. Decline
        // every pending approval, save the approval mode so flipping back can
        // restore it, then force approve (never auto) for the rest of chat mode.
        const approvals = get().pendingApprovals;
        for (const a of approvals) {
          await get().respondApproval(a.tool_call_id, pickRejectOption(a.options));
        }
        set({ pendingApprovals: [], savedApprovalMode: get().mode });
        await ensureSafeApprovalMode();
        // Any agentic-mode path references would leak into a chat-mode prompt —
        // route them through the same inline-content path a chat-mode drop uses.
        const files = get().droppedFiles;
        if (files.length) {
          set({ droppedFiles: [] });
          for (const f of files) await inlineFileAsAttachment(f);
        }
      } else if (wasChatMode && !nowChatMode) {
        // chat → agentic: history/tools are already live goosed-side; just
        // restore whatever approval mode was in effect before the flip to chat.
        const saved = get().savedApprovalMode;
        if (sessionId && saved) await get().setMode(saved);
        set({ savedApprovalMode: null });
      }
    },

    setThinkingEffort: async (value: string) => {
      const sessionId = get().sessionId;
      if (!sessionId) return;
      try {
        const thinkingEffort = await ipc.setThinkingEffort(sessionId, value);
        set({ thinkingEffort });
      } catch (e) {
        set({ error: String(e) });
      }
    },

    addDroppedPaths: async (paths: string[]) => {
      if (!paths.length) return;
      // Placeholder chips (FileChips) so a slow inspect/read/session-create
      // isn't silently invisible — cleared in `finally` regardless of outcome.
      set((s) => ({ pendingAttachments: [...s.pendingAttachments, ...paths] }));
      try {
        let infos = await ipc.inspectPaths(paths);
        // The active model can't see images at all — drop them here, before
        // they ever reach droppedFiles/inlineFileAsAttachment, rather than
        // sending a picture a text-only model will just fail (or silently
        // ignore) on. Non-image files in the same drop still go through.
        if (!supportsImages(get().model)) {
          const rejected = infos.filter((f) => !f.is_dir && isImageFileName(f.name));
          if (rejected.length) {
            infos = infos.filter((f) => !rejected.includes(f));
            set({
              warning: `The active model doesn't support images — skipped: ${rejected.map((f) => f.name).join(', ')}.`,
            });
          }
        }
        if (!infos.length) return;
        // Untrusted-provider warning (Round-2 item 13) — non-blocking, never bans.
        const { providerTier, isTrusted, providerHost } = get();
        if (providerTier && providerTier !== 'local' && !isTrusted) {
          set({
            warning: `Attaching files will send their contents to ${providerHost ?? 'an untrusted provider'}, which you haven't marked trusted.`,
          });
        }
        // Chat mode (Phase 9 / Round-4 toggle): inline file *content* rather
        // than sending paths. Any file type is accepted now (item 13).
        if (isChatMode(get())) {
          for (const f of infos) await inlineFileAsAttachment(f);
          return;
        }
        // Agentic: hand paths to the filesystem tools (works for any file type).
        set((s) => {
          const seen = new Set(s.droppedFiles.map((f) => f.path));
          return { droppedFiles: [...s.droppedFiles, ...infos.filter((f) => !seen.has(f.path))] };
        });
      } catch (e) {
        set({ error: String(e) });
      } finally {
        set((s) => ({
          pendingAttachments: s.pendingAttachments.filter((p) => !paths.includes(p)),
        }));
      }
    },

    removeDroppedPath: (path: string) =>
      set((s) => ({ droppedFiles: s.droppedFiles.filter((f) => f.path !== path) })),

    setWorkingDir: async (folder: string) => {
      // Agentic mode only (the folder-chip button that calls this doesn't
      // render in chat mode at all): repoints the *current* session's cwd
      // in place instead of forking a new session, so BigTiny's directory
      // sandbox (bigtiny/agent/sandbox.py) can allow both the session's
      // original chat_dir and this newly-set directory at once — forking
      // would otherwise leave a brand-new session that never knew the
      // original folder, defeating the whole "chat_dir + set context dir"
      // allowance. Falls back to creating a session if somehow called with
      // none active yet (defensive; not a reachable path from the UI today).
      const s = get();
      if (!s.sessionId) {
        await get().newSession(folder);
        return;
      }
      try {
        await ipc.setSessionContextDir(s.sessionId, folder);
        set({ cwd: folder });
      } catch (e) {
        set({ error: String(e) });
      }
    },

    addPastedText: (text: string, label?: string) =>
      set((s) => ({
        attachments: [
          ...s.attachments,
          {
            id: newId(),
            label: label ?? `Pasted text — ${text.trim().split(/\s+/).length} words`,
            content: text,
          },
        ],
      })),

    removeAttachment: (id: string) =>
      set((s) => ({ attachments: s.attachments.filter((a) => a.id !== id) })),

    addPendingImage: (mime: string, dataUrl: string) => {
      if (!supportsImages(get().model)) {
        set({ warning: "The active model doesn't support images — the image wasn't attached." });
        return;
      }
      set((s) => ({
        pendingImages: [...s.pendingImages, { id: newId(), mime, data_url: dataUrl }],
      }));
    },

    removePendingImage: (id: string) =>
      set((s) => ({ pendingImages: s.pendingImages.filter((p) => p.id !== id) })),

    exportSession: async (upToIndex?: number) => {
      const { messages, title } = get();
      if (messages.length === 0) return;
      const chatMessages = buildExport(messages, upToIndex);
      const base = sanitizeFilename(title ?? 'kitty-session');
      const path = await pickSavePath(`${base}.jsonl`);
      if (!path) return;
      try {
        await ipc.writeFile(path, JSON.stringify({ messages: chatMessages }) + '\n');
      } catch (e) {
        set({ error: String(e) });
      }
    },

    branch: async (uiIndex: number) => {
      const { sessionId, cwd, title, modeOverride } = get();
      if (!sessionId || !cwd) return;
      try {
        // Keep history up to and including the clicked message, diverge after.
        const info = await ipc.forkSession(sessionId, cwd, uiIndex + 1);
        // A fork is a brand-new session id with no persisted mode override of
        // its own — carry the current one forward (same conversation from the
        // user's perspective), mirroring send()'s stripReasoning swap. Must be
        // awaited *before* loadSession, which reads it straight back via
        // getSessionMode — a fire-and-forget write here would race that read.
        if (modeOverride) {
          await ipc.setSessionMode(info.session_id, modeOverride).catch((e) => {
            // Not fatal — loadSession still proceeds — but the fork silently
            // loses its carried-forward mode override and falls back to
            // default mode detection instead, a real behavior difference.
            console.warn('setSessionMode failed on branch, mode override may not carry over', e);
          });
        }
        set({ title: title ? `Branch of ${title}` : 'Branch' });
        await get().loadSession(info.session_id, info.cwd, get().title ?? undefined);
      } catch (e) {
        set({ error: String(e) });
      }
    },

    regenerate: async (assistantIndex: number) => {
      const { sessionId, messages, busy } = get();
      // `sendInFlight` gate too: send()/sendWithRecipe() commit `busy` only
      // after their own awaits, so without it a regenerate could slip in and
      // start a second turn while the first is still being prepared.
      if (!sessionId || busy || sendInFlight) return;
      const target = messages[assistantIndex];
      if (!target || target.role !== 'assistant') return;
      // Find the user message preceding this assistant turn.
      let userIdx = assistantIndex - 1;
      while (userIdx >= 0 && messages[userIdx].role !== 'user') userIdx--;
      if (userIdx < 0) return;
      const userText = messages[userIdx].text;

      // Stay in the *same* session/turn history (owner direction) — Goose has
      // no ACP method to edit or drop a past turn in place, so the response
      // being regenerated away from is collapsed client-side (like the
      // thinking container) rather than removed, and the model is simply
      // asked again with a note to reconsider. No fork, no session swap, no
      // deleted session — this is the same conversation continuing, not a
      // branch.
      clearStopGrace();
      discardDeltas();
      toolLoopCounts = new Map();
      set((s) => {
        const msgs = s.messages.slice();
        msgs[assistantIndex] = { ...msgs[assistantIndex], superseded: true };
        return {
          messages: msgs,
          busy: true,
          error: null,
          errorType: null,
          providerOffline: false,
          stopPhase: null,
          abandonedSession: null,
          loopSuspected: false,
          activeRecipeTurn: null,
        };
      });
      try {
        lastSentAt = performance.now();
        lastSentProvider = get().providerName;
        lastSentModel = get().model;
        // The note is sent to the model as part of the real turn (goosed has
        // no "silent" side channel), but isn't rendered as a second visible
        // user bubble — the collapsed box above already makes clear what's
        // being reconsidered.
        const promptText = `${userText}\n\n(Please reconsider your previous answer above and provide an improved response.)`;
        await ipc.sendPrompt(sessionId, promptText);
      } catch (e) {
        set({ busy: false, error: String(e) });
      }
    },

    send: async (text: string) => {
      const trimmed = text.trim();
      const attachments = get().attachments;
      const pendingImages = get().pendingImages;
      if (
        sendInFlight ||
        get().busy ||
        (!trimmed && attachments.length === 0 && pendingImages.length === 0)
      ) {
        // Don't let a pending recipe card outlive an aborted send (e.g. a
        // recipe invoked while a turn was somehow already in flight) — it
        // would otherwise leak into the next unrelated message.
        if (get().pendingRecipeCard) set({ pendingRecipeCard: null });
        return;
      }
      // Acquire the synchronous in-flight guard BEFORE any await — `busy` is
      // only committed later (after ensureSession/image reads below), so a
      // rapid double-submit (double-Enter) would otherwise both pass this gate
      // and enqueue two prompts.
      sendInFlight = true;
      try {
        await doSend(trimmed);
      } finally {
        sendInFlight = false;
      }
    },

    sendWithRecipe: async (recipe: Recipe, primaryText: string) => {
      // Same synchronous guard as send()/regenerate() — held across the whole
      // prepare-then-send sequence below (ensureSession, extension-adds, mode
      // flip), so a concurrent plain send can't slip in between.
      if (sendInFlight || get().busy) return;
      const missing = recipeNeedsAttention(recipe);
      if (missing.length) {
        set({
          error: `"${recipe.title}" has unconfigured parameters (${missing.join(', ')}) — fix them in Settings → Recipes.`,
        });
        return;
      }
      const { resolvedInstructions, resolvedPromptText } = resolveRecipe(recipe, primaryText);
      // A recipe with no `prompt` template invoked with no trailing text
      // resolves to an empty prompt. `send('')` would early-return (no content)
      // *without* consuming `pendingRecipeCard`, leaking it into the next
      // message and silently dropping the recipe — so guarantee a non-empty
      // driving message. The recipe's real instructions still ride along in the
      // hidden `<recipe>` card; this is just the visible bubble / kick-off text.
      const promptToSend = resolvedPromptText.trim() || `Run the "${recipe.title}" recipe.`;
      sendInFlight = true;
      try {
        const sessionId = await get().ensureSession(); // current session if one
        // exists, else lazily creates one — the exact call send() itself makes;
        // a mid-conversation invocation and a blank-chat invocation need no
        // special-casing between them.
        for (const ext of launchableExtensions(recipe.extensions)) {
          void ipc.addRecipeExtension(sessionId, ext).catch((e) => {
            // The model's later tool calls for this extension will surface
            // their own "not found" errors either way, but log here too so a
            // launch failure (vs. e.g. a genuinely missing tool) is
            // distinguishable in dev tools.
            console.warn(`addRecipeExtension failed for "${ext.name}"`, e);
          });
        }
        let flippedToAgentic = false;
        if (isChatMode(get())) {
          // Recipes are inherently tool-using — flip to agentic so declared/
          // default extensions can actually execute, reusing the existing
          // chat<->agentic flip verbatim (it already handles approval-mode
          // save/restore and pending-approval cleanup correctly).
          await get().setModeOverride('agentic');
          flippedToAgentic = true;
        }
        set({
          pendingRecipeCard: {
            title: recipe.title,
            instructions: resolvedInstructions ?? '',
            maxReasoningTokens: recipe.max_reasoning_tokens,
          },
        });
        const submitted = await doSend(promptToSend);
        if (!submitted) {
          // The turn never actually started (session creation failed or the
          // send aborted) — undo the side effects this recipe already applied
          // so they don't leak into a later unrelated message: drop the
          // pending card and flip back out of agentic if we flipped in.
          set({ pendingRecipeCard: null });
          if (flippedToAgentic) void get().setModeOverride('chat');
        }
      } finally {
        sendInFlight = false;
      }
    },

    bindEvents: () => {
      if (bound) return;
      bound = true;

      // Recipe list — fetched once, kept fresh via the changed-event. Read by
      // the composer's slash-command matching (Recipes settings panel fetches
      // independently, same as scheduled tasks don't share a list either).
      const refreshRecipes = () =>
        // Best-effort: a failure just leaves the slash-command list stale
        // until the next onRecipesChanged event or window reload.
        void ipc
          .listRecipes()
          .then((recipes) => set({ recipes }))
          .catch(() => {});
      refreshRecipes();
      void onRecipesChanged(refreshRecipes);

      // Events count only for the active session, and never for a turn the user
      // force-stopped (Round-5) — that abandonment is cleared when the next
      // send() starts a fresh turn on the session.
      const forActive = (sid: string) => get().sessionId === sid && get().abandonedSession !== sid;

      // Append a text/reasoning chunk to the open message of `role`, opening a new
      // one (and closing any prior open message) when the turn changes.
      const appendChunk = (role: 'user' | 'assistant', field: 'text' | 'reasoning', text: string) =>
        set((s) => {
          const msgs = s.messages.slice();
          const last = msgs[msgs.length - 1];
          if (last && last.role === role && last.open) {
            msgs[msgs.length - 1] = { ...last, [field]: last[field] + text };
            return { messages: msgs };
          }
          const closed = closeOpen(msgs);
          closed.push({
            id: newId(),
            role,
            text: field === 'text' ? text : '',
            reasoning: field === 'reasoning' ? text : '',
            toolCalls: [],
            streaming: role === 'assistant',
            open: true,
          });
          return { messages: closed };
        });

      void onMessageDelta((e) => {
        if (forActive(e.session_id)) bufferDelta('text', e.text);
      });
      void onReasoningDelta((e) => {
        if (forActive(e.session_id)) bufferDelta('reasoning', e.text);
      });
      void onUserMessage((e) => {
        if (!forActive(e.session_id)) return;
        flushDeltas(); // keep a replayed user turn after any buffered agent text
        // Strip any client-side prompt preamble before it's stored in
        // Message.text so a resumed session doesn't show it raw. The recipe
        // wrapper can be on ANY turn (a recipe is invokable mid-conversation),
        // so it's stripped from every user message; the system-prompt/
        // strip-reasoning wrappers only ever wrap a session's first turn (see
        // stripPromptPreamble's doc comment). On a recipe-invoked first turn
        // the recipe wrapper is outermost, so strip it first, then the rest.
        const isFirst = get().messages.length === 0;
        const withoutRecipe = stripRecipeWrapper(e.text);
        appendChunk(
          'user',
          'text',
          stripInternalMarkers(
            isFirst ? stripPromptPreamble(withoutRecipe) : withoutRecipe
          )
        );
      });

      void onToolCall((e) => {
        if (!forActive(e.session_id)) return;
        flushDeltas(); // tool cards must land after the streamed text before them

        const u: ToolCallUpdate = e.update;
        const id = String(u.toolCallId ?? '');
        const artifact = deriveArtifact(u, get().cwd);
        set((s) => {
          let msgs = s.messages.slice();
          let last = msgs[msgs.length - 1];
          if (!(
            last &&
            last.role === 'assistant' &&
            (last.open || isStragglerAssistantMessage(last, s.busy))
          )) {
            msgs = closeOpen(msgs);
            last = {
              id: newId(),
              role: 'assistant',
              text: '',
              reasoning: '',
              toolCalls: [],
              streaming: true,
              open: true,
            };
            msgs.push(last);
          } else {
            last = { ...last };
            msgs[msgs.length - 1] = last;
          }
          const toolCalls = last.toolCalls.slice();
          const existing = toolCalls.findIndex((t) => t.id === id);
          const prev = existing >= 0 ? toolCalls[existing] : undefined;
          const meta = extractGooseToolMeta(u);
          const merged: ToolCall = {
            id,
            title: prev?.title ?? String(u.title ?? u.kind ?? 'tool'),
            status:
              u.status != null
                ? String(u.status)
                : (prev?.status ?? (e.phase === 'tool_call' ? 'pending' : 'running')),
            input: u.rawInput ?? prev?.input,
            output: u.rawOutput ?? u.content ?? prev?.output,
            toolName: prev?.toolName ?? meta.toolName,
            extensionName: prev?.extensionName ?? meta.extensionName,
          };
          if (existing >= 0) toolCalls[existing] = merged;
          else toolCalls.push(merged);
          last.toolCalls = toolCalls;

          const artifacts =
            artifact && !s.artifacts.some((a) => a.path === artifact.path)
              ? [...s.artifacts, artifact]
              : s.artifacts;
          return { messages: msgs, artifacts };
        });
      });

      void onSessionTitle((e) => {
        if (forActive(e.session_id)) set({ title: e.title });
      });

      void onProviderHealth((h) => {
        set({ providerOffline: !h.reachable, providerHost: h.host ?? get().providerHost });
      });
      // Provider (de)activated → re-sync provider-derived state immediately so the
      // UI doesn't drift until the next session or health tick (Round-2 item 4).
      // Also clear a stale "can't reach the provider" banner — whatever failed
      // belonged to the old provider, and activate_provider itself now
      // health-gates the switch, so the newly-active one is already known-good.
      //
      // Deliberately NOT rebinding/reloading this window's session here:
      // provider is per-session now (each session's metadata carries its own
      // provider/model, resolved at send time). activate_provider already
      // stamped the *invoking* window's session with the new provider, so this
      // handler only refreshes the badge state — a broadcast-based rebind of
      // every open window's session is exactly the cross-window leak it used
      // to cause (pick a provider in window 2 → window 1 switched too).
      void onProviderActivated(() => {
        set({ providerOffline: false });
        void get().refreshProvider();
      });
      // A session was deleted (any window, e.g. the sidebar's kebab menu) —
      // if it's the one *this* window currently has open, drop into a blank
      // "new chat" state rather than leaving the chat view pointed at a now-
      // nonexistent session (same reset shape onSessionsCleared uses below;
      // no new goosed session needed, ensureSession() lazily makes one on the
      // next send()).
      void onSessionDeleted((sessionId) => {
        if (get().sessionId === sessionId) {
          recentOwnSessionDeleteAt = Date.now();
          clearStopGrace();
          discardDeltas();
          set({
            sessionId: null,
            cwd: null,
            chatDir: null,
            title: null,
            mode: null,
            availableModes: [],
            thinkingEffort: null,
            messages: [],
            artifacts: [],
            droppedFiles: [],
            attachments: [],
            pendingImages: [],
            pendingAttachments: [],
            pendingApprovals: [],
            modeOverride: null,
            savedApprovalMode: null,
            error: null,
            errorType: null,
            providerOffline: false,
            busy: false,
            stopPhase: null,
            abandonedSession: null,
            loopSuspected: false,
            activeRecipeTurn: null,
          });
        }
      });
      // A completion/failure notification was clicked for a session no window
      // was currently bound to (the window that had it switched to a
      // different chat before the task finished) — Rust picked exactly this
      // window as the fallback target (`windows::focus_or_open_session`);
      // reload the session the same way Expand's handoff does.
      void onAdoptSession((info) => {
        void get().adoptSession(info);
      });
      // "Clear all chat history" (Settings → General) run from any window —
      // blank this window's chat too, if it had one open (same reset shape
      // handOffToMain uses; no new goosed session needed, ensureSession()
      // lazily makes one on the next send()).
      void onSessionsCleared(() => {
        if (get().sessionId) {
          clearStopGrace();
          discardDeltas();
          set({
            sessionId: null,
            cwd: null,
            chatDir: null,
            title: null,
            mode: null,
            availableModes: [],
            thinkingEffort: null,
            messages: [],
            artifacts: [],
            droppedFiles: [],
            attachments: [],
            pendingImages: [],
            pendingAttachments: [],
            pendingApprovals: [],
            modeOverride: null,
            savedApprovalMode: null,
            error: null,
            errorType: null,
            providerOffline: false,
            busy: false,
            stopPhase: null,
            abandonedSession: null,
            loopSuspected: false,
            activeRecipeTurn: null,
          });
        }
      });
      // Clipboard-to-Kitty hotkey/tray item (Round-4): the overlay is already
      // shown by the time this fires (Rust dispatches show_overlay first).
      void onClipboardAttach((e) => {
        if (e.kind === 'text') get().addPastedText(e.text, 'Clipboard');
        else get().addPendingImage(e.mime, e.data_url);
      });
      void onApprovalNeeded((e) => {
        if (!forActive(e.session_id)) return;
        // Auto-decide against the session's allowed directories in BOTH
        // modes (Round-5, owner decision for chat mode; extended to agentic
        // mode alongside BigTiny's own directory-sandboxing feature) — a
        // path-based file op is auto-approved only if its target resolves
        // inside one of `dirs`, auto-rejected otherwise; a tool with no
        // structured path — notably `shell`, which is how the model produces
        // docx/xlsx via Python — is allowed. This is purely a round-trip-
        // avoidance nicety: BigTiny enforces the real containment
        // server-side regardless (`bigtiny/agent/sandbox.py`), so a wrong
        // guess here just costs one extra approval round-trip, never a
        // security gap. `dirs` is the chat folder only in chat mode (cwd
        // never diverges there — no UI path to change it) and the chat
        // folder plus current cwd in agentic mode (which can diverge via
        // "Set as working directory"; see `chatDir`'s own doc comment for
        // the one known imprecision this introduces). This handler only
        // ever fires for a call BigTiny's own HITL policy already decided
        // needs a human (`chat://tool-approval-needed`, i.e. a real
        // `hitl_pause`) — under the default `always_ask` policy that's every
        // tool call, so this auto-decide pass is what keeps tool use feeling
        // seamless in practice, in both modes now, rather than prompting
        // for everything.
        const s0 = get();
        const dirs = isChatMode(s0) ? [s0.cwd] : [s0.chatDir, s0.cwd];
        // Tool-loop guard (owner-reported bug): a model can get stuck
        // alternating tools (e.g. web-fetch ↔ its own cache step) against
        // the same target — each call is real network/disk I/O, so this
        // must be checked *before* deciding to allow it, not after.
        const title = String(e.tool_call.title ?? e.tool_call.kind ?? 'tool');
        const { count, counts } = countToolCall(toolLoopCounts, title, e.tool_call.rawInput);
        toolLoopCounts = counts;
        if (count > TOOL_LOOP_THRESHOLD) {
          void ipc.respondPermission(e.tool_call_id, pickRejectOption(e.options)).catch((err) => {
            // If this never reaches the backend, the paused tool call has no
            // way to resolve and the turn hangs waiting for a decision that
            // was already made on this side.
            console.warn('respondPermission (tool-loop reject) failed', err);
          });
          set({
            warning:
              `Declined — "${title}" has been called ${count} times with the same target ` +
              `this turn. The model appears stuck in a loop; try Force Stop if it doesn't ` +
              `recover on its own.`,
          });
          return;
        }
        const { decision, optionId, warning } = decideChatApproval(
          e.tool_call.rawInput,
          dirs,
          e.options
        );
        if (decision === 'prompt') {
          // Ambiguous enough to need a human — queue it for the real
          // ApprovalPrompt UI instead of auto-deciding, and only NOW (not on
          // every hitl_pause) tell Rust to fire the "Approval needed"
          // toast/tray-pending state — this is the one branch where a human
          // is genuinely required.
          void ipc.notifyApprovalNeeded(e.session_id, title).catch((err) => {
            // Non-fatal — pendingApprovals below still shows the in-app
            // ApprovalPrompt — but the OS notification/tray-pending state
            // this exists for may not fire, so a hidden window could miss it.
            console.warn('notifyApprovalNeeded failed', err);
          });
          set((s) =>
            s.pendingApprovals.some((a) => a.tool_call_id === e.tool_call_id)
              ? {}
              : { pendingApprovals: [...s.pendingApprovals, e] }
          );
          return;
        }
        void ipc.respondPermission(e.tool_call_id, optionId).catch((err) => {
          // Same risk as the tool-loop reject case above: a paused tool call
          // with no way to resolve hangs the turn.
          console.warn('respondPermission (auto-decide) failed', err);
        });
        if (warning) set({ warning });
      });

      void onComplete((e) => {
        // WS8: a backgrounded turn (one this window abandoned via New Chat /
        // switching sessions, which we deliberately keep running) finished
        // while we moved on — drop the "still running" indicator and surface a
        // completion toast whose action reopens that chat and scrolls to the
        // new output (loadSession replays + auto-scrolls on the message list).
        const bg = get().backgroundSession;
        if (bg && e.session_id === bg.sessionId) {
          set({ backgroundSession: null, backgroundTurnToast: { ...bg, ok: true } });
        }
        if (!forActive(e.session_id)) return;
        flushDeltas(); // apply any buffered tail before stamping the final message
        // The turn actually ended — cancel any pending Stop→Force-Stop escalation.
        clearStopGrace();
        const durationMs = lastSentAt != null ? performance.now() - lastSentAt : undefined;
        lastSentAt = null;
        const providerName = lastSentProvider ?? undefined;
        const model = lastSentModel ?? undefined;
        lastSentProvider = null;
        lastSentModel = null;
        const usage = e.result.usage;
        const ttftMs = e.result.timing?.ttftMs;
        // Read before the set() below clears it — see `pendingForcedAnswer`'s
        // doc comment: there's no ACP way to redirect a generation already in
        // flight to its final answer, so a reasoning-cap cancel instead waits
        // for the cancelled turn to actually finish (here, or onChatError
        // below — cancelling can surface as either) and then sends a forced
        // follow-up turn asking for an answer directly.
        const forcedAnswerSession = get().pendingForcedAnswer;
        set((s) => {
          const msgs = closeOpen(s.messages);
          const lastIdx = msgs.length - 1;
          if (lastIdx >= 0 && msgs[lastIdx].role === 'assistant') {
            msgs[lastIdx] = {
              ...msgs[lastIdx],
              durationMs,
              inputTokens: usage?.inputTokens,
              outputTokens: usage?.outputTokens,
              cacheReadTokens: usage?.cacheReadTokens,
              cacheCreationTokens: usage?.cacheCreationTokens,
              ttftMs,
              providerName,
              model,
            };
          }
          return {
            busy: false,
            stopPhase: null,
            pendingApprovals: [],
            messages: msgs,
            loopSuspected: false,
            activeRecipeTurn: null,
            pendingForcedAnswer: null,
          };
        });
        if (forcedAnswerSession === e.session_id) void get().send(FORCED_ANSWER_PROMPT);
      });
      void onChatError((e) => {
        // Same backgrounded-turn handling as onComplete — a backgrounded turn
        // that FAILED should still surface so the user knows their abandoned
        // chat didn't quietly succeed.
        const bg = get().backgroundSession;
        if (bg && e.session_id === bg.sessionId) {
          set({ backgroundSession: null, backgroundTurnToast: { ...bg, ok: false } });
        }
        if (!forActive(e.session_id)) return;
        flushDeltas();
        clearStopGrace();
        const forcedAnswerSession = get().pendingForcedAnswer;
        set((s) => ({
          busy: false,
          stopPhase: null,
          error: e.message,
          errorType: e.error_type ?? null,
          pendingApprovals: [],
          messages: closeOpen(s.messages),
          loopSuspected: false,
          activeRecipeTurn: null,
          pendingForcedAnswer: null,
        }));
        // Cancelling due to the reasoning cap can surface as an error rather
        // than a clean completion — still worth asking for an answer, since
        // the model's own prior reasoning is still available as context.
        if (forcedAnswerSession === e.session_id) void get().send(FORCED_ANSWER_PROMPT);
      });
      void onCompaction((e) => {
        if (!forActive(e.session_id)) return;
        const state = e.memory_slots?.current_state;
        set({
          compactionNotice: state
            ? `Context compacted — memory updated: ${state}`
            : 'Older context was compacted into a background summary.',
        });
      });
    },
  };
});
