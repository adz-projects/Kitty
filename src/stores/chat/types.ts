// Plain data types shared across the chat store and its helper modules.

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
  /** Prompt-cache stats (Anthropic `cache_read_input_tokens`/
      `cache_creation_input_tokens`, or OpenAI-style `prompt_tokens_details.
      cached_tokens`) — absent (not 0) whenever the provider/model doesn't
      report them, same completeness caveat as the other metrics fields. */
  cacheReadTokens?: number;
  cacheCreationTokens?: number;
  /** Time to first token, from BigTiny's `llm_timing` event — the call that
      produced this message's final visible text. Same completeness caveat
      as the other metrics fields: only set on a message from a live send(). */
  ttftMs?: number;
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
      file the user attached to a message, `'disk'` for a file found in the
      working directory that wasn't otherwise derived from a tool call or
      attachment (e.g. dropped in via Explorer) — distinguishes the sources in
      the UI without changing how any of them are opened/revealed. */
  source?: 'user' | 'tool' | 'disk';
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
  availableModes: import('@/lib/types').ModeInfo[];
  thinkingEffort: import('@/lib/types').ThinkingEffort | null;
}

/** Map of tool-call "signature" (see `toolCallSignature`) to how many times
    it's been seen this turn — backs the chat-mode tool-loop guard. */
export type ToolCallCounts = Map<string, number>;
