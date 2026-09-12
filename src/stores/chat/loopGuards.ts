// Guards against a model getting stuck: verbatim reasoning/text repetition and
// a leaked literal `<think>` tag.
//
// The reasoning hard cap that used to live here went with recipes. It was
// driven entirely by a recipe's `max_reasoning_tokens`, and with no producer
// for that number it could never fire again -- along with the forced-answer
// follow-up that only a cap-triggered cancel ever scheduled.
//
// The tool-call loop guard went too. It counted identical calls in a turn and
// had the frontend decline the fifth, which stopped being a loop detector once
// the lean readers and writers went paged: `lean_file_read`,
// `lean_pdf_read_text` and `lean_doc_read_chunk` are meant to be called once
// per window, so reading a long document end to end is a legitimate run of
// identical-looking calls and the guard cut it off mid-document. It also only
// ever ran for calls that paused for approval. The daemon now nudges instead
// -- after several consecutive calls to one tool it appends a note to the tool
// result asking the model to check whether it already has what it needs (see
// `REPEAT_TOOL_NUDGE_AFTER` in `BigTinyV2/daemon/src/agent/loop_.rs`). That
// reaches the only party who can tell "still paging" from "stuck", and it
// cannot strand a turn halfway through a document.

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
