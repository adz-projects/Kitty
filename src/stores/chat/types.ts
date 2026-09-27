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
  /** The result shown is cut short; the whole of it can be fetched by
      `daemonToolCallId` ("Show full output"). */
  truncated?: boolean;
  daemonToolCallId?: string;
}

export interface Message {
  id: string;
  role: 'user' | 'assistant';
  text: string;
  reasoning: string;
  toolCalls: ToolCall[];
  /** An answer the model wrote before all its specialist reports were in.
      Moved out of `text` when the daemon collects those reports for it (see
      `isAutoSpecialistCollection`), so the bubble holds only the real answer
      and export never includes the draft. */
  draftText?: string;
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
  /** Generation speed for that same call, as measured by BigTiny.
      Deliberately not derived here from `outputTokens`/`durationMs`: those
      describe the *whole turn* on the browser's clock — every LLM step, tool
      call, memory recall and DB write — while the token count and the decode
      window belong to one provider call. Dividing one by the other understated
      the real rate roughly threefold on a tool-using turn. */
  tokensPerSecond?: number;
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
  /** Things the user should know about how this reply was produced — a
      model failover, the step limit — shown under it (`chat://notice`). */
  notices?: string[];
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
