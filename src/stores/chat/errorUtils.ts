// Error humanization and prompt-preamble/wrapper stripping for replayed
// messages.

import type { Message } from './types';

/** Turn a raw ACP/JSON-RPC error string into a short, plain-language summary
    for the chat error banner — the raw text stays available via ErrorDetail's
    "Show details" expander. Owner-reported bug: a bare "Invalid params" (or
    similar wire-protocol text) showed up with no explanation of what
    happened or what to do about it. Pattern-matched, not exhaustive — an
    unrecognized error still gets a generic-but-plain fallback rather than
    the raw string as the headline.

    `errorType`, when present, is BigTiny's own classification of a
    `provider_error` event (see `classify_provider_error` on the backend) and
    takes priority over the string-matching below, which exists for legacy/
    unclassified errors (transport failures, cancellations, etc.). */
export function humanizeChatError(raw: string, errorType?: string): string {
  if (errorType === 'context_exceeded') {
    return "The conversation has exceeded the model's context limit. Try starting a new session or enabling compaction to summarize older messages.";
  }
  if (errorType === 'insufficient_credits') {
    return "Your API credits are exhausted. Check your provider's billing settings or switch to another provider.";
  }
  const r = raw.toLowerCase();
  if (r.includes('timed out')) {
    return 'The response took too long and Kitty gave up waiting. Try sending again.';
  }
  if (r.includes('invalid params')) {
    return "Kitty couldn't send that message — this can happen right after switching providers or restarting the engine. Try sending again.";
  }
  if (
    r.includes('connection closed') ||
    r.includes('connection cancelled') ||
    r.includes("isn't running") ||
    r.includes('connect')
  ) {
    return "Lost the connection to Kitty's engine. Kitty will reconnect automatically — try sending again.";
  }
  return 'Something went wrong sending that message.';
}

// STOPGAP client-side workaround for stripping reasoning from resent context
// (see the doc comment on `ProviderProfile.strip_reasoning` in providers.rs and
// on `stripReasoning` in chatStore) — flattens prior turns into plain text
// using only `.text`, never `.reasoning`. Remove once Goose ships a native
// hook (https://github.com/block/goose/issues/7617) and this whole mechanism
// goes away in favor of an env var through goosed_env().
export function buildStrippedTranscript(messages: Message[]): string {
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
// `firstMessage` gate in chatStore. `userMsg.text` (the live-rendered bubble)
// is always built independently of the wrapped `promptText`, so a *live* send
// never shows the wrapper. But goosed stores exactly what was transmitted,
// wrapper included — so resuming a session via `session/load` replays the raw
// wrapped text as that turn's `user_message_chunk`, with nothing in the
// replay path to strip it. Only the first replayed user turn of a session can
// ever carry a wrapper; later turns pass through untouched.
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
    present. */
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
    trade-off as the other wrappers (cosmetic, not a security boundary). */
export function stripRecipeWrapper(text: string): string {
  const m = text.match(RECIPE_WRAPPER_RE);
  return m ? text.slice(m[0].length) : text;
}

// Header lines that introduce backend-injected context blocks. These are all
// delivered as `role: "system"` on the wire and are never persisted or
// surfaced by BigTiny, but `stripInternalMarkers` is a client-side
// defense-in-depth net: if any ever leaks into displayed text (e.g. a model
// echoing the prompt tail back), it must not show the marker or its block.
const INTERNAL_MARKERS = [
  '[Earlier context from this session]',
  '[Adaptive Pathway hints]',
  '[CONSOLIDATED PROJECT MEMORY]',
];

/** Strip any backend-injected context block (headed by one of the internal
    markers above) from `text` — removes the marker header line plus every
    line that follows it until a blank line or the next marker header, so the
    whole injected block disappears cleanly. A no-op when no internal marker
    is present. */
export function stripInternalMarkers(text: string): string {
  let out = text;
  for (const marker of INTERNAL_MARKERS) {
    const escaped = marker.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const re = new RegExp(`(^|\\n)[ \\t]*${escaped}[^\\n]*(?:\\n(?!\\n|\\s*\\[)[^\\n]*)*`, 'g');
    out = out.replace(re, '$1');
  }
  // Collapse the blank-line trail a removed block leaves behind, and trim.
  return out
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{2,}/g, '\n')
    .replace(/^\n+/, '')
    .replace(/\n+$/, '');
}
