// Error humanization and prompt-preamble/wrapper stripping for replayed
// messages.

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
  if (errorType === 'auth_failed') {
    return 'This provider rejected the API key. Check it in Settings — it may be wrong, expired, or revoked.';
  }
  if (errorType === 'network_unreachable') {
    return "Kitty couldn't reach this provider. Check your connection (or the provider's own status) and try sending again.";
  }
  if (errorType === 'turn_in_progress') {
    return 'This chat is still finishing its previous reply. Wait for it to end (or stop it), then send again.';
  }
  if (errorType === 'idle_timeout') {
    return 'The provider stopped responding partway through, so Kitty stopped waiting. Try sending again; a slow local server may need a longer wait in its provider settings.';
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

/** Error types that describe the *provider*, not this conversation — so they
    stop being true the moment a different provider is activated. Everything
    else (a context overflow, a malformed request) survives the switch,
    because switching providers didn't fix it. */
const PROVIDER_SCOPED_ERROR_TYPES = new Set([
  'network_unreachable',
  'auth_failed',
  'insufficient_credits',
]);

export function isProviderScopedError(errorType?: string | null): boolean {
  return !!errorType && PROVIDER_SCOPED_ERROR_TYPES.has(errorType);
}

/** Is this error one that "the provider is reachable again" actually
    resolves?

    Kitty shows connectivity failures twice: the `providerOffline` banner
    (health-tick driven, self-clearing) and the `ErrorDetail` card from the
    failed turn. Only the banner had a path back to healthy, so a card
    reading "Can't reach provider" sat there after the connection came back,
    reporting a problem that no longer existed.

    Kept deliberately narrow. A timeout is not included: the provider
    answering a health probe says nothing about whether the request that
    timed out would now succeed, so that card stays until the user retries
    and finds out. */
export function isConnectivityError(raw: string | null, errorType?: string | null): boolean {
  if (!raw) return false;
  // A classified error means the backend already decided what this is;
  // don't second-guess it by also string-matching the raw text.
  if (errorType) return errorType === 'network_unreachable';
  const r = raw.toLowerCase();
  return (
    r.includes('connection closed') ||
    r.includes('connection cancelled') ||
    r.includes("isn't running") ||
    r.includes('connect')
  );
}

// Both historical client-side prompt-preamble wrappers (the Round-6 system
// prompt, and the since-removed "strip reasoning" option's reconstructed
// transcript) were only ever prepended to the FIRST outgoing message of a
// session. Nothing produces either any more, but sessions recorded before they
// were removed still carry them: the daemon stores exactly what was
// transmitted, wrapper included, so replaying such a session would show the
// raw wrapped text in its first user bubble. Only the first replayed user turn
// of a session can carry one; later turns pass through untouched.
const SYSTEM_PROMPT_WRAPPER_RE = /^<system>\n[\s\S]*?\n<\/system>\n\n/;
const TRANSCRIPT_WRAPPER_PREAMBLE =
  'Continuing the conversation below. Earlier reasoning/thinking has been omitted ' +
  'to keep this response focused.\n\n';
// Closing line of every transcript the removed strip-reasoning option built. The
// send-time wrapper is exactly `<transcript>\n\n<sentinel>\n\nUser: <text>`,
// so the replay strip anchors on this exact suffix boundary — a bare
// `lastIndexOf('\n\nUser: ')` could match inside the user's OWN message
// (e.g. a pasted chat log) and eat its head. Model-sensible text, not a
// control sequence, since it rides along in the prompt the model sees.
const TRANSCRIPT_SENTINEL = '[End of earlier conversation]';
// The hidden `<recipe>…</recipe>` + "Run the recipe above now…" wrapper the
// removed recipes feature used to prepend. Nothing produces one any more, but
// sessions recorded before specialists replaced recipes still carry it, and a
// replay that stopped stripping it would start showing raw markup in old
// transcripts. `[^>]*` tolerates any title attribute content
// (except a literal `>`); the lazy `[\s\S]*?` stops at the first
// `\n</recipe>`; `[^>]*\n\n` after it consumes the single-line run
// instruction. Unlike the system/transcript wrappers (first turn only), a
// a recipe could be invoked on ANY turn, so this is stripped from every
// replayed user message — see `stripRecipeWrapper`'s use below.
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
    // Exact send-time boundary first (see TRANSCRIPT_SENTINEL): only the
    // wrapper's own trailing `<sentinel>\n\nUser: ` is unambiguous — anything
    // after it is the user's message verbatim, even if it contains further
    // "\n\nUser: " sequences of its own.
    const anchor = `${TRANSCRIPT_SENTINEL}\n\nUser: `;
    const anchored = rest.lastIndexOf(anchor);
    if (anchored >= 0) return rest.slice(anchored + anchor.length);
    // Legacy fallback for sessions persisted before the sentinel existed:
    // keep the old last-marker heuristic (imperfect — a user message
    // containing the literal marker loses its head — but those transcripts
    // carry no better boundary to anchor on).
    const marker = '\n\nUser: ';
    const idx = rest.lastIndexOf(marker);
    if (idx >= 0) return rest.slice(idx + marker.length);
  }
  return text;
}

/** Strip the historical `<recipe>` wrapper from a replayed user message, if
    present. Kept for transcripts recorded before specialists replaced recipes.
    Separate from `stripPromptPreamble` because a recipe could be
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
/** One compiled regex per marker, built once at module load instead of on
    every call. `stripInternalMarkers` runs at the single render chokepoint for
    user turns, so this used to compile three regexes per render of any user
    message containing a marker. `INTERNAL_MARKERS` is a module constant, so
    there is nothing per-call to key them on. */
const MARKER_RES = INTERNAL_MARKERS.map((marker) => {
  const escaped = marker.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`(^|\\n)[ \\t]*${escaped}[^\\n]*(?:\\n(?!\\n|\\s*\\[)[^\\n]*)*`, 'g');
});

export function stripInternalMarkers(text: string): string {
  // No marker present? Skip the whole regex sweep and, crucially, the
  // trailing whitespace-collapse — that trimming would otherwise eat
  // intentional leading line breaks from plain user messages on every replay.
  if (!INTERNAL_MARKERS.some((m) => text.includes(m))) return text;
  let out = text;
  for (const re of MARKER_RES) {
    // These are `g` regexes and now shared across calls. `String.replace`
    // resets `lastIndex` itself, but reset explicitly rather than depending
    // on that from a module-level object.
    re.lastIndex = 0;
    out = out.replace(re, '$1');
  }
  // Collapse the blank-line trail a removed block leaves behind, and trim.
  return out
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{2,}/g, '\n')
    .replace(/^\n+/, '')
    .replace(/\n+$/, '');
}
