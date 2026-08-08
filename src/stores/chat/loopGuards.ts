// Guards against a model getting stuck: verbatim reasoning/text repetition,
// an over-length recipe reasoning cap, a leaked literal `<think>` tag, and a
// tool-call loop (same call, same target, repeatedly) in chat mode.

import type { ToolCallCounts } from './types';

export type { ToolCallCounts };

/** Sent automatically once a reasoning-cap-triggered cancel actually
    completes (see `pendingForcedAnswer` in chatStore) — there's no way to
    redirect an in-flight generation straight to its answer, so this asks for
    one on a fresh turn instead of leaving the user with nothing. */
export const FORCED_ANSWER_PROMPT =
  'Based on the work you did in your previous turn, please produce a response.';

/** Rough English-text chars-per-token approximation, used only to enforce a
    recipe's `max_reasoning_tokens` hard cap client-side (see `flushDeltas` in
    chatStore) — ACP exposes no numeric reasoning-token count to check
    against, only effort levels, so this is the best available proxy, not an
    exact count. */
const CHARS_PER_TOKEN_ESTIMATE = 4;

/** Pure decision function behind the recipe reasoning hard cap — separated
    from `flushDeltas`'s zustand `get()`/`set()` plumbing so the actual
    threshold math is unit-testable on its own, same pattern as
    `hasRepetitionLoop`. */
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

/** Best-effort identifying string for a tool call — tool name/kind plus its
    primary argument (URL, path, or command; falls back to the whole input).
    Not a full hash, just enough to tell "the same call, again" apart from "a
    different call." */
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

/** The `target` half of a tool call's signature (URL/path/command), without
    the tool title. Used by the alternation detector below — two *different*
    tools against the *same* target need a shared key to recognize an
    alternating loop. */
export function toolCallTarget(rawInput: unknown): string | null {
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
  return typeof primary === 'string' ? primary : null;
}

/** Alternation-tracking state for the tool-loop guard. Keyed by target so two
    different tools against the same target share one counter. */
export type ToolAlternationState = Map<string, { lastTitle: string | null; flips: number }>;

/** Pure updater behind the *alternating*-tools half of the tool-loop guard.
    The per-signature count in `countToolCall` alone can't catch a model that
    bounces between two tools (web-fetch ↔ its own caching step) on the same
    target — each tool's own count stays below the threshold forever. This
    instead tracks consecutive tool-title *changes* for each target; a
    sustained A→B→A→B… sequence keeps incrementing `flips` against the shared
    target key, so the guard can flag it. A repeated same-tool call leaves
    `flips` at 0 and is left to the per-signature counter. */
export function trackToolAlternation(
  state: ToolAlternationState,
  title: string,
  rawInput: unknown
): { flips: number; state: ToolAlternationState } {
  const target = toolCallTarget(rawInput);
  if (!target) return { flips: 0, state };
  const next = new Map(state);
  const cur = next.get(target) ?? { lastTitle: null, flips: 0 };
  const flips = cur.lastTitle !== null && cur.lastTitle !== title ? cur.flips + 1 : 0;
  next.set(target, { lastTitle: title, flips });
  return { flips, state: next };
}

/** Chat-mode tool-loop guard (owner-reported bug): a model can get stuck
    alternating between two tools against the same target (e.g. a web-fetch
    tool and its own cache step) — each iteration a real network/disk
    round-trip, not just wasted tokens, since goose actually executes the call
    before this fires. Increments and returns the new count for this call's
    signature. Pure (counts passed in/out, a fresh Map returned) so it's
    unit-testable and resettable per turn — see `send()` in chatStore, which
    clears the live counts at the start of every fresh turn; repeating a call
    across different turns is normal, not a loop. */
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
export const TOOL_LOOP_THRESHOLD = 4;
