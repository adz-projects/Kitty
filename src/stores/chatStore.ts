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
  onSessionTitle,
  onToolCall,
  onUserMessage,
  pickSavePath,
  windowLabel,
} from '@/lib/ipc';
import { buildExport, sanitizeFilename } from '@/lib/chatml';
import { defaultSystemPrompt } from '@/lib/system_prompts';
import type {
  ApprovalNeededEvent,
  ModeInfo,
  NetworkTier,
  PathInfo,
  ToolCallUpdate,
} from '@/lib/types';

export interface ToolCall {
  id: string;
  title: string;
  status: string;
  input?: unknown;
  output?: unknown;
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
}

export interface Artifact {
  path: string;
  name: string;
  tool: string;
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
  messages: Message[];
  artifacts: Artifact[];
  droppedFiles: PathInfo[];
  attachments: Attachment[];
  pendingImages: PendingImage[];
  pendingApprovals: ApprovalNeededEvent[];
  busy: boolean;
  error: string | null;
  // Active-provider derived state (Phase 9/10)
  toolsEnabled: boolean;
  providerTier: NetworkTier | null;
  providerHost: string | null;
  providerOffline: boolean;
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
      follow the active provider's `tools_enabled` default — see `isChatMode`. */
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
  bindEvents: () => void;
  dismissWarning: () => void;
  /** Clear the Artifacts pane list (Round-5). Only empties the derived
      in-memory list — never touches the files on disk. */
  clearArtifacts: () => void;
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
  loadSession: (sessionId: string, cwd: string, title?: string) => Promise<void>;
  reloadCurrent: () => Promise<void>;
  send: (text: string) => Promise<void>;
  cancel: () => Promise<void>;
  /** Hard-reset a stuck turn the user chose to force-stop (Round-5): clears
      `busy`, ends the in-flight message, and abandons the turn so late events
      are ignored. Only meaningful once `stopPhase === 'forceable'`. */
  forceStop: () => void;
  respondApproval: (toolCallId: string, optionId: string | null) => Promise<void>;
  setMode: (modeId: string) => Promise<void>;
  /** Flip the session's chat/agentic mode (Round-4). Persists the override and
      handles the mid-conversation safety/attachment concerns described on
      `send()`'s strip-reasoning STOPGAP neighbor, `bindEvents`' approval
      handler, and `addDroppedPaths` below. */
  setModeOverride: (mode: 'chat' | 'agentic' | null) => Promise<void>;
  addDroppedPaths: (paths: string[]) => Promise<void>;
  removeDroppedPath: (path: string) => void;
  setWorkingDir: (folder: string) => Promise<void>;
  adoptSession: (info: {
    session_id: string;
    cwd: string;
    current_mode: string;
    available_modes: ModeInfo[];
  }) => Promise<void>;
  /** Hand the current session off to the main window (the overlay's "Expand"
      button and the chat-only first-message auto-promote both use this).
      Resets local session state afterward so reopening the overlay lands on
      a blank composer instead of the just-expanded conversation (Round-4
      item 7) — no new goosed session is created here; `ensureSession()`
      lazily makes one on the overlay's next send(). */
  handOffToMain: () => Promise<void>;
}

/** Effective chat/agentic mode for the current session: an explicit override
    wins, otherwise follow the active provider's `tools_enabled` default.
    Exported plain selector — usable as `useChatStore(isChatMode)` in
    components or `isChatMode(get())` inside store actions (Round-4). */
export const isChatMode = (s: ChatState): boolean =>
  (s.modeOverride ?? (s.toolsEnabled ? 'agentic' : 'chat')) === 'chat';

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

export function deriveArtifact(u: ToolCallUpdate, cwd: string | null = null): Artifact | null {
  const meta = u as Record<string, unknown>;
  const goose = (meta._meta as { goose?: { toolCall?: { toolName?: string } } })?.goose;
  const toolName = goose?.toolCall?.toolName ?? '';
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
  };
}

const closeOpen = (msgs: Message[]): Message[] =>
  msgs.map((m) => (m.open ? { ...m, open: false, streaming: false } : m));

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
  // which Round-5 now permits inside the chat folder.) Binaries can't be
  // inlined as text, so they attach as a short descriptor instead.
  const inlineFileAsAttachment = async (f: PathInfo) => {
    if (f.is_dir) return;
    try {
      const file = await ipc.readFileAny(f.path);
      if (file.kind === 'text') {
        get().addPastedText(file.content, f.name);
      } else {
        get().addPastedText(
          `[Attached file "${f.name}" — ${file.mime ?? 'binary'}; contents not inlined.]`,
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
      if (last && last.role === 'assistant' && last.open) {
        msgs[msgs.length - 1] = { ...last, text: last.text + t, reasoning: last.reasoning + r };
        return { messages: msgs };
      }
      const closed = closeOpen(msgs);
      closed.push({
        id: newId(),
        role: 'assistant',
        text: t,
        reasoning: r,
        toolCalls: [],
        streaming: true,
        open: true,
      });
      return { messages: closed };
    });
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
  };

  return {
    sessionId: null,
    cwd: null,
    title: null,
    mode: null,
    availableModes: [],
    messages: [],
    artifacts: [],
    droppedFiles: [],
    attachments: [],
    pendingImages: [],
    pendingApprovals: [],
    busy: false,
    error: null,
    toolsEnabled: true,
    providerTier: null,
    providerHost: null,
    providerOffline: false,
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

    dismissWarning: () => set({ warning: null }),

    clearArtifacts: () => set({ artifacts: [] }),

    refreshProvider: async () => {
      try {
        const providers = await ipc.listProviders();
        const active = providers.find((p) => p.active);
        set({
          toolsEnabled: active ? active.tools_enabled : true,
          providerTier: active ? active.network_tier : null,
          providerHost: active ? new URL(active.base_url).host : null,
          isTrusted: active ? active.is_trusted : false,
          model: active?.models[0] ?? null,
          providerName: active ? active.name || active.provider_type : null,
          stripReasoning: active ? active.strip_reasoning : false,
          systemPrompt: active ? active.system_prompt : null,
        });
      } catch {
        set({
          toolsEnabled: true,
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
    },

    handOffToMain: async () => {
      const s = get();
      if (s.sessionId) {
        await ipc.setActiveSession({
          session_id: s.sessionId,
          cwd: s.cwd ?? '',
          current_mode: s.mode ?? 'auto',
          available_modes: s.availableModes,
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
        messages: [],
        artifacts: [],
        droppedFiles: [],
        attachments: [],
        pendingImages: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
        busy: false,
        stopPhase: null,
        abandonedSession: null,
      });
    },

    newSession: async (cwd?: string) => {
      const info = await ipc.newSession(cwd);
      clearStopGrace();
      discardDeltas();
      set({
        sessionId: info.session_id,
        cwd: info.cwd,
        mode: info.current_mode,
        availableModes: info.available_modes,
        title: null,
        messages: [],
        artifacts: [],
        attachments: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
        busy: false,
        stopPhase: null,
        abandonedSession: null,
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

    loadSession: async (sessionId: string, cwd: string, title?: string) => {
      // Set the id first so replayed events (which arrive during the call) match.
      // Clear any force-stop abandonment up front — otherwise reloading a
      // previously force-stopped session would drop its replay events.
      clearStopGrace();
      discardDeltas();
      set({
        sessionId,
        cwd,
        title: title ?? null,
        messages: [],
        artifacts: [],
        pendingApprovals: [],
        modeOverride: null,
        savedApprovalMode: null,
        error: null,
        busy: true,
        stopPhase: null,
        abandonedSession: null,
      });
      try {
        const info = await ipc.loadSession(sessionId, cwd);
        set({ mode: info.current_mode, availableModes: info.available_modes });
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
        set((s) => ({ busy: false, messages: closeOpen(s.messages) }));
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
      }));
    },

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

    addDroppedPaths: async (paths: string[]) => {
      if (!paths.length) return;
      try {
        const infos = await ipc.inspectPaths(paths);
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

    addPendingImage: (mime: string, dataUrl: string) =>
      set((s) => ({
        pendingImages: [...s.pendingImages, { id: newId(), mime, data_url: dataUrl }],
      })),

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
      const { sessionId, cwd, title } = get();
      if (!sessionId || !cwd) return;
      try {
        // Keep history up to and including the clicked message, diverge after.
        const info = await ipc.forkSession(sessionId, cwd, uiIndex + 1);
        set({ title: title ? `Branch of ${title}` : 'Branch' });
        await get().loadSession(info.session_id, info.cwd, get().title ?? undefined);
      } catch (e) {
        set({ error: String(e) });
      }
    },

    regenerate: async (assistantIndex: number) => {
      const { sessionId, cwd, messages } = get();
      if (!sessionId || !cwd) return;
      // Find the user message preceding this assistant turn.
      let userIdx = assistantIndex - 1;
      while (userIdx >= 0 && messages[userIdx].role !== 'user') userIdx--;
      if (userIdx < 0) return;
      const userText = messages[userIdx].text;
      try {
        // Fork + truncate to just before the user turn so the original response is
        // preserved in the parent session; then resend.
        const info = await ipc.forkSession(sessionId, cwd, userIdx);
        await get().loadSession(info.session_id, info.cwd, get().title ?? undefined);
        await get().send(userText);
      } catch (e) {
        set({ error: String(e) });
      }
    },

    send: async (text: string) => {
      const trimmed = text.trim();
      const attachments = get().attachments;
      const pendingImages = get().pendingImages;
      if ((!trimmed && attachments.length === 0 && pendingImages.length === 0) || get().busy) {
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
      // Chat-only mode is unaffected here — its binary-attachment stub is a
      // separate, lower-priority concern (no tool-invocation failure mode exists
      // there since chat-only never calls tools).
      const isImage = (name: string) => /\.(png|jpe?g|gif|webp|bmp)$/i.test(name);
      const imageFiles = !chatOnly ? files.filter((f) => !f.is_dir && isImage(f.name)) : [];
      const otherFiles = files.filter((f) => !imageFiles.includes(f));
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
      const cwd = get().cwd ?? undefined;

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
        stopPhase: null,
        abandonedSession: null,
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
        // Chat-only: auto-promote the *first* overlay message to the full window.
        if (chatOnly && firstMessage && windowLabel() === 'overlay') {
          await get().handOffToMain();
        }
      } catch (e) {
        set({ busy: false, error: String(e) });
      }
    },

    bindEvents: () => {
      if (bound) return;
      bound = true;

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
        appendChunk('user', 'text', e.text);
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
          if (!(last && last.role === 'assistant' && last.open)) {
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
          const merged: ToolCall = {
            id,
            title: prev?.title ?? String(u.title ?? u.kind ?? 'tool'),
            status:
              u.status != null
                ? String(u.status)
                : (prev?.status ?? (e.phase === 'tool_call' ? 'pending' : 'running')),
            input: u.rawInput ?? prev?.input,
            output: u.rawOutput ?? u.content ?? prev?.output,
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
      void onProviderActivated(() => void get().refreshProvider());
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
          return { busy: false, stopPhase: null, pendingApprovals: [], messages: msgs };
        });
      });
      void onChatError((e) => {
        if (!forActive(e.session_id)) return;
        flushDeltas();
        clearStopGrace();
        set((s) => ({
          busy: false,
          stopPhase: null,
          error: e.message,
          pendingApprovals: [],
          messages: closeOpen(s.messages),
        }));
      });
    },
  };
});
