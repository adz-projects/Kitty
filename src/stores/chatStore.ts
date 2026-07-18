// Chat render state. Assembled live from `chat://*` events; the durable
// conversation lives in goosed (CLAUDE.md rule 3). The assembly is turn-aware so
// it handles both live prompting and full-conversation replay on session/load.

import { create } from 'zustand';
import {
  ipc,
  onApprovalNeeded,
  onChatError,
  onClipboardAttach,
  onComplete,
  onMessageDelta,
  onMode,
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
import { tryParsePyRepr } from '@/lib/pyrepr';
import { recipeNeedsAttention, resolveRecipe, launchableExtensions } from '@/lib/recipes';
import { supportsImages } from '@/lib/vision_models';
import type {
  ApprovalNeededEvent,
  ModeInfo,
  NetworkTier,
  PathInfo,
  ProviderType,
  ProviderView,
  Recipe,
  SessionInfo,
  ThinkingEffort,
  ToolCallUpdate,
} from '@/lib/types';

export interface ToolCall {
  id: string;
  title: string;
  status: string;
  input?: unknown;
  output?: unknown;
  /** `_meta.goose.toolCall.{toolName,extensionName}` (docs/acp-protocol.md) —
      lets the chat surface recognize a specific extension's tool call, e.g.
      the Adaptive Pathway extension's `decide` (Round-C). */
  toolName?: string;
  extensionName?: string;
}

export interface Message {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  reasoning: string;
  toolCalls: ToolCall[];
  streaming: boolean;
  /** Currently being appended to (internal to the assembly). */
  open: boolean;
  // Per-response metrics (Round-3 item 2) — only set on a message that just
  // completed via a live send() in this session; replayed/resumed messages
  // (session/load) never go through send_prompt's completion path, so they
  // won't have these, which is expected.
  durationMs?: number;
  inputTokens?: number;
  outputTokens?: number;
  providerName?: string;
  /** The actual model that generated this message (Round-4 info button) —
      captured at send time, not read back from the live chat-pill state. */
  model?: string;
  /** Files/images attached to this turn (Round-7 fix): a snapshot taken at
      send() time, before droppedFiles/attachments/pendingImages are cleared
      from composer state — without this, a message with both typed text and
      an attachment showed no trace of the attachment at all once sent. Only
      set on a message that just completed via a live send() in this session;
      like the metrics fields above, a replayed/resumed message won't have
      this (goosed's stored history has no structured "what was attached"
      metadata to reconstruct it from — a known, accepted limitation). */
  attachedFiles?: { name: string; kind: 'file' | 'document' | 'image' }[];
  /** Set by `regenerate()`: this assistant turn was superseded by a
      reconsidered answer right after it, in the same session — rendered
      collapsed (like the thinking container) instead of as a normal bubble. */
  superseded?: boolean;
}

export interface Artifact {
  path: string;
  name: string;
  tool: string;
  /** `'tool'` (default) for goosed tool-call-derived artifacts, `'user'` for a
      file the user attached to a message — distinguishes the two sources in
      the UI without changing how either is opened/revealed. */
  source?: 'user' | 'tool';
}

/** An inlined document (large paste or dropped text file) in chat-only mode. */
export interface Attachment {
  id: string;
  label: string;
  content: string;
}

/** An image attached directly (not via a dropped file path) — currently just
    the clipboard hotkey (Round-4). Sent as a native ACP image content block
    in both modes (Round-3 item 17's mechanism isn't agentic-only; only the
    droppedFiles-based image extraction below happens to be, since it's about
    file drops specifically). */
export interface PendingImage {
  id: string;
  mime: string;
  data_url: string;
}

interface ChatState {
  sessionId: string | null;
  cwd: string | null;
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
  /** Clear the Artifacts pane list (Round-5). Only empties the derived
      in-memory list — never touches the files on disk. */
  clearArtifacts: () => void;
  /** Drop any artifact whose file no longer exists on disk — covers both a
      tool call deleting it and the user deleting it out-of-band (Explorer,
      another app). Best-effort: a failed check just leaves the list as-is
      until the next call. */
  pruneMissingArtifacts: () => Promise<void>;
  refreshProvider: () => Promise<void>;
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
// above) — module-level like `stopGraceTimer`, reset at the start of every
// fresh turn in `send()`.
let toolLoopCounts: ToolCallCounts = new Map();

// Last-known mode/effort info per provider, cached in localStorage (shared
// across windows — same webview origin) so a *brand-new* window's very first
// "New Chat" can show `EffortDropdown`/`ModeBadge` immediately instead of
// waiting on the real `session/new` round trip. `newSession()`'s existing
// same-window carry-forward (see its doc comment) only helps the 2nd+ session
// in a window's lifetime, since there's nothing to carry forward before any
// session has ever been created there — this cache covers that gap. A fresh
// session on the same provider/model will almost always report the same
// values, so this is a reasonable optimistic seed; the real response is
// still authoritative and overwrites it the moment it lands.
export interface CachedModeInfo {
  mode: string;
  availableModes: ModeInfo[];
  thinkingEffort: ThinkingEffort | null;
}
export const modeInfoCacheKey = (providerId: string) => `kitty:lastModeInfo:${providerId}`;
export function readCachedModeInfo(providerId: string): CachedModeInfo | null {
  try {
    const raw = localStorage.getItem(modeInfoCacheKey(providerId));
    return raw ? (JSON.parse(raw) as CachedModeInfo) : null;
  } catch {
    return null;
  }
}
export function writeCachedModeInfo(providerId: string, info: CachedModeInfo) {
  try {
    localStorage.setItem(modeInfoCacheKey(providerId), JSON.stringify(info));
  } catch {
    // best-effort — a full/unavailable localStorage just means no seed next time
  }
}

// Dedupes concurrent session-creation requests (Round-7): `newSession()`
// clears the UI optimistically before awaiting `ipc.newSession`, so a
// concurrent `send()` (user types+sends before that await resolves) sees
// `sessionId === null` and calls `ensureSession()` → `newSession()` again.
// Without this, that would fire a second real `ipc.newSession` call and
// orphan one of the two goosed sessions. Module-level like the fields above.
let pendingNewSession: Promise<SessionInfo> | null = null;
function getOrCreateSession(cwd?: string): Promise<SessionInfo> {
  if (!pendingNewSession) {
    pendingNewSession = ipc.newSession(cwd).finally(() => {
      pendingNewSession = null;
    });
  }
  return pendingNewSession;
}

// A file-writing tool by name/verb. Broadened (Round-5) beyond the original
// text_editor set to cover the write verbs other tools use.
const ARTIFACT_RE =
  /text_editor|write|create|edit|str_replace|insert|append|save|export|output|generate/i;
// A recognized artifact file by extension — the second, independent signal
// (Round-5): a tool that exposes an output path ending in one of these counts
// as an artifact even when its name doesn't match a write verb (e.g. a
// document/spreadsheet-producing tool). Covers the formats owners asked for
// (csv/xlsx/docx/md/json/py) plus common neighbors.
const ARTIFACT_EXT_RE =
  /\.(csv|tsv|xlsx?|xlsm|docx?|pptx?|md|markdown|json|jsonl|ya?ml|py|txt|html?|xml|pdf|rtf|odt|ods|odp|ipynb|sql|toml)$/i;
// Explicit read/inspect operations that also carry a `path` (e.g. text_editor
// `command:"view"`) — excluded so opening/reading a file never fabricates an
// artifact. This also fixes a latent false positive: `text_editor` matches
// ARTIFACT_RE by name, so a plain view used to register as a (bogus) artifact.
const READ_COMMAND_RE = /^(view|read|list|open|cat|show|inspect|search|find|glob|grep)$/i;

/** Make a tool-reported output path absolute so the Artifacts pane's Open /
    Copy path / Show-in-folder work (Round-5): goose reports a *relative* path
    for a write (e.g. `report.docx`), resolved against the session cwd — the
    chat folder. Without this the artifact stored just the bare filename and
    Open failed with "file not found". Absolute inputs (drive letter, unix
    root, UNC) are kept as-is. */
function absoluteArtifactPath(p: string, cwd: string | null): string {
  const isAbsolute = /^[a-z]:[\\/]/i.test(p) || p.startsWith('/') || p.startsWith('\\\\');
  if (isAbsolute || !cwd) return p;
  return `${cwd.replace(/[\\/]+$/, '')}/${p.replace(/^[\\/]+/, '')}`;
}

/** `_meta.goose.toolCall.{toolName,extensionName}` (docs/acp-protocol.md) —
    shared by `deriveArtifact` and the `onToolCall` handler's `ToolCall`
    assembly, since both need to know which tool/extension a call belongs to. */
export function extractGooseToolMeta(u: ToolCallUpdate): {
  toolName?: string;
  extensionName?: string;
} {
  const meta = u as Record<string, unknown>;
  const goose = (
    meta._meta as { goose?: { toolCall?: { toolName?: string; extensionName?: string } } }
  )?.goose;
  return {
    toolName: goose?.toolCall?.toolName,
    extensionName: goose?.toolCall?.extensionName,
  };
}

export function deriveArtifact(u: ToolCallUpdate, cwd: string | null = null): Artifact | null {
  const toolName = extractGooseToolMeta(u).toolName ?? '';
  const label = `${u.title ?? ''} ${toolName}`;
  const input = u.rawInput as
    { path?: string; file_path?: string; paths?: string[]; command?: string } | undefined;
  const p =
    input?.path ?? input?.file_path ?? (Array.isArray(input?.paths) ? input?.paths[0] : undefined);
  if (typeof p !== 'string' || !p) return null;
  // Never derive an artifact from an explicit read/view of a file.
  if (typeof input?.command === 'string' && READ_COMMAND_RE.test(input.command)) return null;
  // Qualify on either signal: a write-like tool name/verb, or a known artifact
  // file extension on the output path.
  if (!ARTIFACT_RE.test(label) && !ARTIFACT_EXT_RE.test(p)) return null;
  return {
    path: absoluteArtifactPath(p, cwd),
    name: p.split(/[\\/]/).pop() || p,
    tool: toolName || String(u.title || 'tool'),
    source: 'tool',
  };
}

/** Registers a user-attached file as an artifact (distinct from
    `deriveArtifact`'s tool-call-derived ones) — same dedup-by-path rule as the
    `onToolCall` handler, so a file that's later also written to by a tool
    doesn't produce a duplicate entry. */
function userFileArtifact(path: string, name: string): Artifact {
  return { path, name, tool: 'attached', source: 'user' };
}

/** A single suggestion from the Adaptive Pathway extension's `decide` tool
    (Round-C). `edge_id` is the "why was this suggested" link target (a
    passthrough fix on the extension side — its own `attribution_id` is
    unrelated/unusable, see that repo's KNOWN_ISSUES.md). */
export interface AdaptivePathwayHint {
  text: string;
  confidence: number;
  type: string;
  edge_id?: string;
  /** Short explanation behind the hint (e.g. "succeeded in 41 contexts;
      confidence 72%"), rendered under the hint badge. `null`/absent when the
      extension has nothing to say beyond the hint text itself. */
  rationale?: string | null;
  /** Which model produced this hint — drives the badge style
      (`standard` = hollow, `ig` = "info gain", `pc` = "paradigm",
      `wildcard` = "untested angle", with a lightbulb icon). Absent on older
      extension versions. */
  source_model?: string;
}

interface ParsedHintOutput {
  hints: AdaptivePathwayHint[];
  confidence: number;
  novelty: number;
  /** True when the extension is offering to widen exploration for the next
      few turns (the exploration-consent prompt). Absent/false on older
      extension versions or when no offer is being made this turn. */
  nudge_offered: boolean;
}

/** Mirrors `goose_provider_name` in `src-tauri/src/config/providers.rs` —
    goosed only ever sees `custom_openai` profiles as a plain `openai`
    provider (same client, just a different base URL), so that's the value
    it reports back in a session's stored `providerId`. */
function gooseProviderName(providerType: ProviderType): string {
  return providerType === 'custom_openai' ? 'openai' : providerType;
}

/** Finds the Kitty provider profile that produced a session's stored
    goosed-level `providerId`/`modelId` (`session/list`'s `_meta`, see
    `SessionSummary`) — used to restore the right provider when reopening an
    old chat. First match wins if the user has multiple profiles of the same
    type with the same model selected; goosed's own metadata has no way to
    disambiguate further than provider type + model id. `null` if nothing
    currently configured matches (e.g. the profile was deleted). */
export function findMatchingProvider(
  providers: ProviderView[],
  providerId: string,
  modelId: string
): ProviderView | null {
  return (
    providers.find(
      (p) => gooseProviderName(p.provider_type) === providerId && p.models.includes(modelId)
    ) ?? null
  );
}

/** Finds the Adaptive Pathway extension's `decide` tool call on a message, if
    any — hints correlate to messages "for free" this way, since `decide` is
    just another tool call on the same streaming assistant message (no new
    cross-message correlation scheme needed). Gated on `toolName` alone for
    now; tighten to also require a specific `extensionName` once confirmed
    live what Goose actually reports for this extension. */
export function findHintToolCall(msg: Message): ToolCall | undefined {
  return msg.toolCalls.find((t) => t.toolName === 'decide');
}

/** Parses a `decide` tool call's `output` (Python-repr text, see `pyrepr.ts`)
    into its hints. Returns `null` on anything unexpected — a malformed or
    still-streaming tool output should never crash message rendering. */
export function parseHintOutput(call: ToolCall | undefined): ParsedHintOutput | null {
  if (!call?.output) return null;
  const parsed = tryParsePyRepr(String(call.output)) as Record<string, unknown> | null;
  if (!parsed || !Array.isArray(parsed.hints)) return null;
  const hints = parsed.hints.filter(
    (h): h is AdaptivePathwayHint =>
      typeof h === 'object' && h !== null && typeof (h as { text?: unknown }).text === 'string'
  );
  if (hints.length === 0) return null;
  return {
    hints,
    confidence: typeof parsed.confidence === 'number' ? parsed.confidence : 0,
    novelty: typeof parsed.novelty === 'number' ? parsed.novelty : 0,
    nudge_offered: parsed.nudge_offered === true,
  };
}

const closeOpen = (msgs: Message[]): Message[] =>
  msgs.map((m) => (m.open ? { ...m, open: false, streaming: false } : m));

/** True when `last` is an already-closed assistant message from the turn that
    *just* finished, with nothing new started yet. A tool-call notification
    and the turn's completion response travel through separate tasks
    (goosed's reader loop resolves the completion oneshot on a different task
    than the one that emits `chat://tool-call`/reasoning/message deltas), so
    it's possible for `chat://complete` to reach the frontend and close the
    message a moment before a straggling event for that same turn arrives.
    Without this, the straggler spawned a brand-new, text-less assistant
    message instead of joining the box already shown above the real answer.
    Safe to fold back in: a genuinely new turn always pushes a user message
    first (via `send()`), so `last.role` would be `'user'` by then, not
    `'assistant'` — this can only match a true straggler. */
export function isStragglerAssistantMessage(last: Message | undefined, turnBusy: boolean): boolean {
  return !!last && last.role === 'assistant' && !last.open && !turnBusy;
}

/** Shared by `send()`'s dropped-file image split and `inlineFileAsAttachment`
    (chat-only mode's attach path) — one definition so the two can't drift. */
export function isImageFileName(name: string): boolean {
  return /\.(png|jpe?g|gif|webp|bmp)$/i.test(name);
}

/** Rough English-text chars-per-token approximation, used only to enforce a
    recipe's `max_reasoning_tokens` hard cap client-side (see `flushDeltas`) —
    ACP exposes no numeric reasoning-token count to check against, only
    effort levels, so this is the best available proxy, not an exact count. */
const CHARS_PER_TOKEN_ESTIMATE = 4;

/** Sent automatically once a reasoning-cap-triggered cancel actually
    completes (see `pendingForcedAnswer`) — there's no way to redirect an
    in-flight generation straight to its answer, so this asks for one on a
    fresh turn instead of leaving the user with nothing. */
const FORCED_ANSWER_PROMPT =
  'Based on the work you did in your previous turn, please produce a response.';

/** Pure decision function behind the recipe reasoning hard cap — separated
    from `flushDeltas`'s zustand `get()`/`set()` plumbing so the actual
    threshold math is unit-testable on its own, same pattern as
    `hasRepetitionLoop`. Exported for unit testing. */
export function exceedsReasoningCap(reasoningLength: number, maxReasoningTokens: number): boolean {
  const approxReasoningTokens = Math.ceil(reasoningLength / CHARS_PER_TOKEN_ESTIMATE);
  return approxReasoningTokens > maxReasoningTokens;
}

/** Detects a real, observed local-model failure mode: instead of ever
    finishing, the model gets stuck repeating a short "planning out loud"
    block near-verbatim dozens of times (seen with a small model looping on
    tool-orchestration self-talk — "I'll use `decide`... I'm ready... I'll
    start." — until some length/turn cap finally cut it off). Checked on a
    bounded trailing window of the accumulated reasoning+text, not the whole
    string, so it stays cheap on a long turn. A `chunkSize`+ run of text
    recurring `minRepeats`+ times verbatim within that window is treated as a
    loop. `chunkSize`/`minRepeats` are deliberately generous (150 chars, 8
    repeats): a real degenerate loop repeats dozens of times, so this still
    catches it comfortably, while a long, legitimate response's occasional
    reused phrase/connective — confirmed real false positive at the original,
    looser 100-char/4-repeat thresholds, specifically on long responses —
    essentially never coincidentally repeats a 150+ char verbatim span that
    many times. Probes are anchored only in the first half of the window so
    there's room left for a real repeat to land. */
export function hasRepetitionLoop(
  text: string,
  windowSize = 4000,
  chunkSize = 150,
  minRepeats = 8
): boolean {
  if (text.length < chunkSize * minRepeats) return false;
  const window = text.slice(-windowSize);
  for (let start = 0; start + chunkSize <= window.length / 2; start += chunkSize) {
    const probe = window.slice(start, start + chunkSize);
    if (probe.trim().length < chunkSize * 0.6) continue; // skip mostly-whitespace probes
    let count = 0;
    let idx = 0;
    for (;;) {
      const next = window.indexOf(probe, idx);
      if (next === -1) break;
      count++;
      idx = next + chunkSize;
      if (count >= minRepeats) return true;
    }
  }
  return false;
}

/** Real, observed failure mode: a model that emits reasoning as literal
    inline `<think>...</think>` tags (rather than via a distinct structured
    API field — e.g. Gemma-family models) sometimes has its stream
    misclassified partway through by goosed/Ollama: the *tail* of the
    thinking — including the model's own literal closing tag — arrives as
    ordinary message-delta content instead of reasoning-delta content, so it
    renders in the visible answer bubble instead of the collapsible thinking
    box (confirmed via a real captured transcript: the rendered answer
    literally contained "(End of thought process)\n</think>" as plain text,
    right before the real answer began). Given the message-channel `text`
    accumulated so far, checks for a literal `</think>` marker; when present,
    splits at the *first* occurrence — everything up to and including the tag
    is reasoning that leaked into the wrong channel, everything after is the
    real answer — stripping a leading `<think>` too, in case the whole thing
    (not just the tail) leaked. Returns `null` when no leaked tag is present,
    so callers can treat that as "nothing to do" for the (overwhelmingly
    common) case where classification worked correctly. */
export function splitLeakedThinkTag(text: string): { reasoning: string; text: string } | null {
  const closeIdx = text.indexOf('</think>');
  if (closeIdx === -1) return null;
  let leaked = text.slice(0, closeIdx);
  const openIdx = leaked.indexOf('<think>');
  if (openIdx !== -1) leaked = leaked.slice(openIdx + '<think>'.length);
  const rest = text.slice(closeIdx + '</think>'.length).replace(/^\s+/, '');
  return { reasoning: leaked.trim(), text: rest };
}

/** The ACP permission options confirmed live are `allow_always`/`allow_once`/
    `reject_once`/`reject_always` (docs/acp-protocol.md) — pick the reject
    variant so an auto-declined tool call reads as a real decline, not a
    cancellation. `null` (cancel) as a fallback if none match. */
const pickRejectOption = (options: { optionId: string }[]): string | null =>
  options.find((o) => /reject/i.test(o.optionId))?.optionId ?? null;

/** Pick the "allow once" variant (never `allow_always`, so approval never
    silently persists) for auto-approving a scoped chat-mode tool call. */
const pickAllowOption = (options: { optionId: string }[]): string | null =>
  options.find((o) => o.optionId === 'allow_once')?.optionId ??
  options.find((o) => /allow/i.test(o.optionId))?.optionId ??
  null;

const normPath = (p: string): string => p.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();

/** Lexically (no fs access) decide whether `target` is inside `base`. Absolute
    targets keep their drive/root; relative ones resolve against `base`; `.`/`..`
    are collapsed. Case-insensitive (Windows). This backs the chat-mode "keep
    file ops inside the chat folder" soft boundary — a lexical check is
    proportionate since shell tools (also allowed in chat mode) aren't
    sandboxed anyway; it hard-confines only the path-based ops Kitty can
    actually inspect. Exported for unit testing. */
export function pathWithinDir(base: string, target: string): boolean {
  const b = normPath(base);
  if (!b) return false;
  let t = target.replace(/\\/g, '/');
  const isAbsolute = /^[a-z]:\//i.test(t) || t.startsWith('/');
  if (!isAbsolute) t = `${b}/${t}`;
  const hasDrive = /^[a-z]:/i.test(t);
  const drive = hasDrive ? t.slice(0, 2) : '';
  const stack: string[] = [];
  for (const seg of (hasDrive ? t.slice(2) : t).split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') stack.pop();
    else stack.push(seg);
  }
  const resolved = normPath(`${drive}/${stack.join('/')}`);
  return resolved === b || resolved.startsWith(`${b}/`);
}

/** Whether `target` sits under Goose's own internal cache directory
    (`.../Block/goose/cache/...`, e.g. `computercontroller`'s scraped-page
    cache). These are the tool's own working storage, not a file the model is
    saving for the user, so they're out of scope for the chat-folder boundary
    entirely — rejecting them just breaks the tool (e.g. web fetch) without
    protecting anything. Lexical, matching `pathWithinDir`'s no-fs-access
    style. Exported for unit testing. */
export function isGooseInternalCachePath(target: string): boolean {
  return /(^|\/)block\/goose\/cache(\/|$)/i.test(target.replace(/\\/g, '/'));
}

/** Map of tool-call "signature" (see `toolCallSignature`) to how many times
    it's been seen this turn — backs the chat-mode tool-loop guard below. */
export type ToolCallCounts = Map<string, number>;

/** Best-effort identifying string for a tool call — tool name/kind plus its
    primary argument (URL, path, or command; falls back to the whole input).
    Not a full hash, just enough to tell "the same call, again" apart from "a
    different call." Exported for unit testing. */
export function toolCallSignature(title: string, rawInput: unknown): string {
  const input = (rawInput ?? {}) as {
    url?: string;
    path?: string;
    file_path?: string;
    paths?: string[];
    command?: string;
  };
  const primary =
    input.url ??
    input.path ??
    input.file_path ??
    (Array.isArray(input.paths) ? input.paths[0] : undefined) ??
    input.command;
  const target = typeof primary === 'string' ? primary : JSON.stringify(input);
  return `${title}::${target}`;
}

/** Chat-mode tool-loop guard (owner-reported bug): a model can get stuck
    alternating between two tools against the same target (e.g. a web-fetch
    tool and its own cache step) — each iteration a real network/disk
    round-trip, not just wasted tokens, since goose actually executes the call
    before this fires. Increments and returns the new count for this call's
    signature. Pure (counts passed in/out, a fresh Map returned) so it's
    unit-testable and resettable per turn — see `send()`, which clears the
    live counts at the start of every fresh turn; repeating a call across
    different turns is normal, not a loop. Exported for unit testing. */
export function countToolCall(
  counts: ToolCallCounts,
  title: string,
  rawInput: unknown
): { count: number; counts: ToolCallCounts } {
  const sig = toolCallSignature(title, rawInput);
  const next = new Map(counts);
  const count = (next.get(sig) ?? 0) + 1;
  next.set(sig, count);
  return { count, counts: next };
}

/** More than this many identical calls in one turn is treated as a stuck
    loop, not legitimate repeated tool use. */
const TOOL_LOOP_THRESHOLD = 4;

/** Decide how to answer a tool-approval request while in chat ("thought-
    partner") mode (Round-5, owner decision): tools are allowed, but a path-
    based file op is confined to the session's chat folder (`cwd`). Returns the
    ACP `optionId` to respond with, plus a `warning` to surface when a request
    is declined for reaching outside the folder. A tool with no structured path
    (notably `shell`, which produces docx/xlsx via Python) is allowed — a soft
    boundary, since shell isn't sandboxed. Pure + exported for unit testing. */
export function decideChatApproval(
  rawInput: unknown,
  cwd: string | null,
  options: { optionId: string }[]
): { optionId: string | null; warning?: string } {
  const input = (rawInput ?? {}) as { path?: string; file_path?: string; paths?: string[] };
  const p =
    input.path ?? input.file_path ?? (Array.isArray(input.paths) ? input.paths[0] : undefined);
  if (
    typeof p === 'string' &&
    p !== '' &&
    !!cwd &&
    !pathWithinDir(cwd, p) &&
    !isGooseInternalCachePath(p)
  ) {
    return {
      optionId: pickRejectOption(options),
      warning:
        `Declined a file operation outside this chat's folder (${p}). In thought-partner ` +
        `mode the model can only touch files inside the chat's own folder.`,
    };
  }
  return { optionId: pickAllowOption(options) };
}

// STOPGAP client-side workaround for stripping reasoning from resent context
// (see the doc comment on `ProviderProfile.strip_reasoning` in providers.rs and
// on `stripReasoning` above) — flattens prior turns into plain text using only
// `.text`, never `.reasoning`. Remove once Goose ships a native hook
// (https://github.com/block/goose/issues/7617) and this whole mechanism goes
// away in favor of an env var through goosed_env().
function buildStrippedTranscript(messages: Message[]): string {
  const lines = messages.map((m) =>
    m.role === 'user' ? `User: ${m.text}` : `Assistant: ${m.text}`
  );
  return (
    'Continuing the conversation below. Earlier reasoning/thinking has been omitted ' +
    'to keep this response focused.\n\n' +
    lines.join('\n\n')
  );
}

// Both known client-side prompt-preamble wrappers (Round-6 system prompt, and
// this STOPGAP's own reconstructed transcript, immediately above) are only
// ever prepended to the FIRST outgoing message of a session — see `send()`'s
// `firstMessage` gate. `userMsg.text` (the live-rendered bubble) is always
// built independently of the wrapped `promptText`, so a *live* send never
// shows the wrapper. But goosed stores exactly what was transmitted, wrapper
// included — so resuming a session via `session/load` replays the raw wrapped
// text as that turn's `user_message_chunk`, with nothing in the replay path to
// strip it. Only the first replayed user turn of a session can ever carry a
// wrapper; later turns pass through untouched.
const SYSTEM_PROMPT_WRAPPER_RE = /^<system>\n[\s\S]*?\n<\/system>\n\n/;
const TRANSCRIPT_WRAPPER_PREAMBLE =
  'Continuing the conversation below. Earlier reasoning/thinking has been omitted ' +
  'to keep this response focused.\n\n';
// The hidden `<recipe>…</recipe>` + "Run the recipe above now…" wrapper
// `sendWithRecipe` prepends. `[^>]*` tolerates any title attribute content
// (except a literal `>`); the lazy `[\s\S]*?` stops at the first
// `\n</recipe>`; `[^>]*\n\n` after it consumes the single-line run
// instruction. Unlike the system/transcript wrappers (first turn only), a
// recipe can be invoked on ANY turn, so this is stripped from every replayed
// user message — see `stripRecipeWrapper`'s use below.
const RECIPE_WRAPPER_RE = /^<recipe\b[^>]*>\n[\s\S]*?\n<\/recipe>\n\n[^\n]*\n\n/;

/** Strip a known prompt-preamble wrapper from a replayed first user message, if
    present — heuristic pattern matching (not perfect: a user message that
    happens to start with `<system>...` or the exact transcript preamble text
    would also get stripped), but this is a client-side cosmetic concern, not a
    security boundary, so a rare false-positive is an acceptable trade for
    hiding the wrapper on replay. Returns `text` unchanged if neither wrapper is
    present. Exported for unit testing. */
/** Turn a raw ACP/JSON-RPC error string into a short, plain-language summary
    for the chat error banner — the raw text stays available via ErrorDetail's
    "Show details" expander. Owner-reported bug: a bare "Invalid params" (or
    similar wire-protocol text) showed up with no explanation of what
    happened or what to do about it. Pattern-matched, not exhaustive — an
    unrecognized error still gets a generic-but-plain fallback rather than
    the raw string as the headline. */
export function humanizeChatError(raw: string): string {
  const r = raw.toLowerCase();
  if (r.includes('timed out')) {
    return 'The response took too long and Kitty gave up waiting. Try sending again.';
  }
  if (r.includes('invalid params')) {
    return "Kitty couldn't send that message — this can happen right after switching providers or restarting Goose. Try sending again.";
  }
  if (
    r.includes('connection closed') ||
    r.includes('connection cancelled') ||
    r.includes("isn't running") ||
    r.includes('connect')
  ) {
    return 'Lost the connection to Goose. Kitty will reconnect automatically — try sending again.';
  }
  return 'Something went wrong sending that message.';
}

export function stripPromptPreamble(text: string): string {
  const systemMatch = text.match(SYSTEM_PROMPT_WRAPPER_RE);
  if (systemMatch) return text.slice(systemMatch[0].length);
  if (text.startsWith(TRANSCRIPT_WRAPPER_PREAMBLE)) {
    const rest = text.slice(TRANSCRIPT_WRAPPER_PREAMBLE.length);
    const marker = '\n\nUser: ';
    const idx = rest.lastIndexOf(marker);
    if (idx >= 0) return rest.slice(idx + marker.length);
  }
  return text;
}

/** Strip the hidden `<recipe>` wrapper from a replayed user message, if
    present. Separate from `stripPromptPreamble` because a recipe can be
    invoked on any turn (not just the first), so this runs on every replayed
    user message; on a recipe-invoked first turn the recipe wrapper is
    outermost (wraps the system-prompt wrapper), so callers strip this first
    and then apply `stripPromptPreamble`. Same acceptable false-positive
    trade-off as the other wrappers (cosmetic, not a security boundary).
    Exported for unit testing. */
export function stripRecipeWrapper(text: string): string {
  const m = text.match(RECIPE_WRAPPER_RE);
  return m ? text.slice(m[0].length) : text;
}

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
        const fixed = resolveThinkLeak(last.text + t, last.reasoning + r);
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

  return {
    sessionId: null,
    cwd: null,
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
    pendingApprovals: [],
    busy: false,
    sessionProviderId: null,
    sessionModelId: null,
    sessionConcluded: false,
    replaying: false,
    error: null,
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
    stopPhase: null,
    abandonedSession: null,
    loopSuspected: false,
    recipes: [],
    pendingRecipeCard: null,
    activeRecipeTurn: null,
    pendingForcedAnswer: null,

    dismissWarning: () => set({ warning: null }),

    clearArtifacts: () => set({ artifacts: [] }),

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

    refreshProvider: async () => {
      try {
        const providers = await ipc.listProviders();
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
      if (s.sessionId) {
        await ipc.setActiveSession({
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
        });
      }
      await ipc.openMain();
      await ipc.hideOverlay();
      // No new goosed session is created here — ensureSession() lazily makes
      // one the next time this (now-blank) overlay actually sends a message.
      clearStopGrace();
      discardDeltas();
      set({
        sessionId: null,
        cwd: null,
        title: null,
        mode: null,
        availableModes: [],
        thinkingEffort: null,
        messages: [],
        artifacts: [],
        droppedFiles: [],
        attachments: [],
        pendingImages: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
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
        cwd: null,
        creatingSession: true,
        title: null,
        messages: [],
        artifacts: [],
        droppedFiles: [],
        attachments: [],
        pendingImages: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
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
      const info = await getOrCreateSession(cwd);
      set({
        sessionId: info.session_id,
        cwd: info.cwd,
        mode: info.current_mode,
        availableModes: info.available_modes,
        thinkingEffort: info.thinking_effort,
        creatingSession: false,
      });
      await get().refreshProvider();
      await ensureSafeApprovalMode();
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
      set({
        sessionId,
        cwd,
        title: title ?? null,
        thinkingEffort: null,
        messages: [],
        artifacts: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
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
        // still around — a provider switch is a full goosed restart (no
        // lighter-weight per-session rebind exists at the ACP layer, see
        // docs/acp-protocol.md's Config surface note), so only do it when
        // actually needed. If nothing currently configured matches, the
        // profile was deleted since this chat was last used — don't touch
        // the active provider; instead mark it concluded so the composer
        // blocks new sends while still showing the history below.
        if (resolvedProviderId && resolvedModelId) {
          const providers = await ipc.listProviders();
          const matched = findMatchingProvider(providers, resolvedProviderId, resolvedModelId);
          if (matched) {
            const active = providers.find((p) => p.active);
            if (!active || active.id !== matched.id) {
              await ipc.activateProvider(matched.id);
            }
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
        await get().refreshProvider();
        const override = await ipc.getSessionMode(sessionId).catch(() => null);
        set({ modeOverride: (override as 'chat' | 'agentic' | null) ?? null });
        await ensureSafeApprovalMode();
      } catch (e) {
        set({ error: String(e) });
      } finally {
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
        messages: closeOpen(s.messages),
        warning: 'Stopped. Goose may still be finishing this turn in the background.',
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
      set((s) => ({
        pendingApprovals: s.pendingApprovals.filter((a) => a.tool_call_id !== toolCallId),
      }));
      try {
        await ipc.respondPermission(toolCallId, optionId);
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
      }
    },

    removeDroppedPath: (path: string) =>
      set((s) => ({ droppedFiles: s.droppedFiles.filter((f) => f.path !== path) })),

    setWorkingDir: async (folder: string) => {
      await get().newSession(folder);
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
      const base = sanitizeFilename(title ?? 'goose-session');
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
          await ipc.setSessionMode(info.session_id, modeOverride).catch(() => {});
        }
        set({ title: title ? `Branch of ${title}` : 'Branch' });
        await get().loadSession(info.session_id, info.cwd, get().title ?? undefined);
      } catch (e) {
        set({ error: String(e) });
      }
    },

    regenerate: async (assistantIndex: number) => {
      const { sessionId, messages, busy } = get();
      if (!sessionId || busy) return;
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
      if ((!trimmed && attachments.length === 0 && pendingImages.length === 0) || get().busy) {
        // Don't let a pending recipe card outlive an aborted send (e.g. a
        // recipe invoked while a turn was somehow already in flight) — it
        // would otherwise leak into the next unrelated message.
        if (get().pendingRecipeCard) set({ pendingRecipeCard: null });
        return;
      }
      const firstMessage = get().messages.length === 0;
      const chatOnly = isChatMode(get());
      const sessionId = await get().ensureSession();
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
        images = [];
        for (const f of imageFiles) {
          try {
            const file = await ipc.readFileAny(f.path);
            images.push({ mime: file.mime ?? 'image/png', data_url: file.content });
          } catch (e) {
            set({ error: String(e) });
          }
        }
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
      // session only — a hidden preamble on the actual outgoing prompt text,
      // never on `userMsg.text` below (built independently from `trimmed`), so
      // the rendered bubble shows only what the user typed. `firstMessage` was
      // captured before the stripReasoning session-swap logic above, so a
      // mid-conversation swap onto a fresh goosed session correctly does NOT
      // get a second prepend — from the user's perspective it's a continuation,
      // not a new conversation.
      if (firstMessage) {
        const resolvedPrompt = get().systemPrompt ?? defaultSystemPrompt(chatOnly);
        promptText = `<system>\n${resolvedPrompt}\n</system>\n\n${promptText}`;
      }
      // Recipe invocation (`sendWithRecipe`) — unlike the system-prompt preamble
      // above, this applies on ANY turn, not just a session's first message: a
      // recipe can be invoked at any point in a conversation, attaching to
      // whatever's already open rather than requiring a fresh session. Placed
      // after the system-prompt block so both can compose on a session's very
      // first turn (recipe instructions take priority for this message, the
      // provider's general system prompt still applies underneath).
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
      try {
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
          const info = await ipc.newSession(cwd);
          set({
            sessionId: info.session_id,
            mode: info.current_mode,
            availableModes: info.available_modes,
          });
          // Carry the mode override across to the new session id (it's the same
          // conversation from the user's perspective) and force a safe approval
          // mode on it, same as any other freshly-established chat-mode session.
          if (oldModeOverride) {
            void ipc.setSessionMode(info.session_id, oldModeOverride).catch(() => {});
          }
          await ensureSafeApprovalMode();
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
        } else {
          await ipc.sendPrompt(sessionId, promptText, images);
        }
        // Chat-only sessions stay in the overlay — no auto-promote to the full
        // window (owner decision: the overlay is a fully capable chat surface
        // on its own; "Expand" remains available for anyone who wants the
        // bigger window, but it's never forced).
      } catch (e) {
        set({ busy: false, error: String(e) });
      }
    },

    sendWithRecipe: async (recipe: Recipe, primaryText: string) => {
      if (get().busy) return; // same gate as send()/regenerate() — avoids doing
      // the extension-add/mode-flip side effects below for a turn that would
      // just no-op in send() anyway.
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
      const sessionId = await get().ensureSession(); // current session if one
      // exists, else lazily creates one — the exact call send() itself makes;
      // a mid-conversation invocation and a blank-chat invocation need no
      // special-casing between them.
      for (const ext of launchableExtensions(recipe.extensions)) {
        void ipc.addRecipeExtension(sessionId, ext).catch(() => {});
      }
      if (isChatMode(get())) {
        // Recipes are inherently tool-using — flip to agentic so declared/
        // default extensions can actually execute, reusing the existing
        // chat<->agentic flip verbatim (it already handles approval-mode
        // save/restore and pending-approval cleanup correctly).
        await get().setModeOverride('agentic');
      }
      set({
        pendingRecipeCard: {
          title: recipe.title,
          instructions: resolvedInstructions ?? '',
          maxReasoningTokens: recipe.max_reasoning_tokens,
        },
      });
      await get().send(promptToSend);
    },

    bindEvents: () => {
      if (bound) return;
      bound = true;

      // Recipe list — fetched once, kept fresh via the changed-event. Read by
      // the composer's slash-command matching (Recipes settings panel fetches
      // independently, same as scheduled tasks don't share a list either).
      const refreshRecipes = () =>
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
        appendChunk('user', 'text', isFirst ? stripPromptPreamble(withoutRecipe) : withoutRecipe);
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
      void onMode((e) => {
        if (forActive(e.session_id)) set({ mode: e.mode });
      });

      void onProviderHealth((h) => {
        set({ providerOffline: !h.reachable, providerHost: h.host ?? get().providerHost });
      });
      // Provider (de)activated → re-sync provider-derived state immediately so the
      // UI doesn't drift until the next session or health tick (Round-2 item 4).
      // Also clear a stale "can't reach the provider" banner — whatever failed
      // belonged to the old provider, and activate_provider itself now
      // health-gates the switch, so the newly-active one is already known-good.
      // Reload the current session too: activating a provider always restarts
      // goosed (a fresh process), which has no in-memory record of whatever
      // session id this window was using — continuing to send() against it
      // as-is produced a raw, unexplained "Invalid params"-class error from
      // goosed. reloadCurrent() re-establishes it via session/load (the same
      // recovery the "Restart Goose" degraded-panel button already uses) —
      // a no-op if there's no active session yet.
      void onProviderActivated(() => {
        set({ providerOffline: false });
        void get().refreshProvider();
        // Switching providers only respawns goosed with new env vars, which
        // only affects a *brand-new* session's default — an already-open
        // session keeps its own previously-bound model. Confirmed real bug:
        // continuing to chat in the same session after switching providers
        // sent the OLD provider's model id to the NEW provider ("... is not
        // a valid model ID"). Best-effort hot-rebind before reloading, so the
        // replay reflects the corrected binding.
        const sid = get().sessionId;
        if (sid) void ipc.rebindSessionProvider(sid).catch(() => {});
        void get().reloadCurrent();
      });
      // A session was deleted (any window, e.g. the sidebar's kebab menu) —
      // if it's the one *this* window currently has open, drop into a blank
      // "new chat" state rather than leaving the chat view pointed at a now-
      // nonexistent session (same reset shape onSessionsCleared uses below;
      // no new goosed session needed, ensureSession() lazily makes one on the
      // next send()).
      void onSessionDeleted((sessionId) => {
        if (get().sessionId === sessionId) {
          clearStopGrace();
          discardDeltas();
          set({
            sessionId: null,
            cwd: null,
            title: null,
            mode: null,
            availableModes: [],
            thinkingEffort: null,
            messages: [],
            artifacts: [],
            droppedFiles: [],
            attachments: [],
            pendingImages: [],
            pendingApprovals: [],
            modeOverride: null,
            savedApprovalMode: null,
            error: null,
            providerOffline: false,
            busy: false,
            stopPhase: null,
            abandonedSession: null,
            loopSuspected: false,
            activeRecipeTurn: null,
          });
        }
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
            title: null,
            mode: null,
            availableModes: [],
            thinkingEffort: null,
            messages: [],
            artifacts: [],
            droppedFiles: [],
            attachments: [],
            pendingImages: [],
            pendingApprovals: [],
            modeOverride: null,
            savedApprovalMode: null,
            error: null,
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
        // Chat ("thought-partner") mode: tools are allowed but scoped to the
        // session's own chat folder (Round-5, owner decision). We still force
        // `approve` mode (ensureSafeApprovalMode) so every tool call surfaces
        // here as a permission request we can *decide*, rather than `auto`
        // executing it unseen. Decision: a path-based file op is auto-approved
        // only if its target resolves inside cwd (the chat folder), and
        // auto-rejected otherwise; a tool with no structured path — notably
        // `shell`, which is how the model produces docx/xlsx via Python — is
        // allowed and runs with cwd = the chat folder. Soft boundary: shell
        // isn't sandboxed, so a command could still reach outside; the path
        // check hard-confines the one class of ops Kitty can actually inspect.
        // (ChatView hides ApprovalPrompt in chat mode, but ThinkingBox still
        // renders the tool cards, so the model's tool use stays visible.)
        if (isChatMode(get())) {
          // Tool-loop guard (owner-reported bug): a model can get stuck
          // alternating tools (e.g. web-fetch ↔ its own cache step) against
          // the same target — each call is real network/disk I/O, so this
          // must be checked *before* deciding to allow it, not after.
          const title = String(e.tool_call.title ?? e.tool_call.kind ?? 'tool');
          const { count, counts } = countToolCall(toolLoopCounts, title, e.tool_call.rawInput);
          toolLoopCounts = counts;
          if (count > TOOL_LOOP_THRESHOLD) {
            void ipc.respondPermission(e.tool_call_id, pickRejectOption(e.options)).catch(() => {});
            set({
              warning:
                `Declined — "${title}" has been called ${count} times with the same target ` +
                `this turn. The model appears stuck in a loop; try Force Stop if it doesn't ` +
                `recover on its own.`,
            });
            return;
          }
          const { optionId, warning } = decideChatApproval(
            e.tool_call.rawInput,
            get().cwd,
            e.options
          );
          void ipc.respondPermission(e.tool_call_id, optionId).catch(() => {});
          if (warning) set({ warning });
          return;
        }
        set((s) =>
          s.pendingApprovals.some((a) => a.tool_call_id === e.tool_call_id)
            ? {}
            : { pendingApprovals: [...s.pendingApprovals, e] }
        );
      });

      void onComplete((e) => {
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
        if (!forActive(e.session_id)) return;
        flushDeltas();
        clearStopGrace();
        const forcedAnswerSession = get().pendingForcedAnswer;
        set((s) => ({
          busy: false,
          stopPhase: null,
          error: e.message,
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
    },
  };
});
