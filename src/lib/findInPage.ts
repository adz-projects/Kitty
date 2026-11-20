// Text search over a rendered chat transcript, for the find bar (Ctrl+F).
// The browser's own find is off (`context_menu.rs`), so Kitty finds itself:
// every case-insensitive occurrence becomes a DOM Range, painted with the
// CSS Custom Highlight API (`::highlight(kitty-find)` in base.css) — no
// markup is added to the messages, so React's tree is never touched.

/** Elements whose text is not part of what a reader searches: controls and
    hidden bits. */
const SKIP = 'button, input, textarea, select, script, style, [hidden], [aria-hidden="true"]';

/** Character offsets of every case-insensitive occurrence of `needle` in
    `haystack`, left to right, not overlapping. Pure. */
export function findOffsets(haystack: string, needle: string): number[] {
  const n = needle.toLocaleLowerCase();
  if (!n) return [];
  const h = haystack.toLocaleLowerCase();
  const out: number[] = [];
  for (let at = h.indexOf(n); at >= 0; at = h.indexOf(n, at + n.length)) out.push(at);
  return out;
}

/** Every occurrence of `query` in the text under `root`, as ranges in
    reading order. A match may not cross from one text node into the next
    (formatting splits words rarely enough that this is the right trade). */
export function findRanges(root: Element, query: string): Range[] {
  const q = query.trim();
  if (!q) return [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode: (node) =>
      node.parentElement?.closest(SKIP) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT,
  });
  const ranges: Range[] = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const text = node.textContent ?? '';
    for (const at of findOffsets(text, q)) {
      const r = document.createRange();
      r.setStart(node, at);
      r.setEnd(node, at + q.length);
      ranges.push(r);
    }
  }
  return ranges;
}

/** Whether this webview paints custom highlights (Chromium 105+). */
function highlightsSupported(): boolean {
  return typeof CSS !== 'undefined' && 'highlights' in CSS && typeof Highlight !== 'undefined';
}

/** Paint `ranges`, with `current` set apart; `[]` clears. */
export function paintMatches(ranges: Range[], current: number) {
  if (!highlightsSupported()) return;
  CSS.highlights.delete('kitty-find');
  CSS.highlights.delete('kitty-find-current');
  if (ranges.length === 0) return;
  CSS.highlights.set('kitty-find', new Highlight(...ranges.filter((_, i) => i !== current)));
  if (ranges[current]) CSS.highlights.set('kitty-find-current', new Highlight(ranges[current]));
}

/** Scroll the current match to the middle of its scroller. */
export function revealMatch(range: Range | undefined) {
  const el = range?.startContainer.parentElement;
  el?.scrollIntoView({ block: 'center', inline: 'nearest' });
}
