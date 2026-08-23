/** Block-splitter for the streaming markdown renderer (perf item S).

    `MessageItem` used to hand the whole of `message.text` to a single
    `<ReactMarkdown>` on every animation frame, so remark re-parsed the entire
    message and rehype-highlight re-highlighted every code block from scratch,
    against text that grows monotonically — O(n) per frame, O(n²) per turn.

    Markdown is block-structured: appending text cannot change an
    already-closed block. So we split the text into top-level blocks and render
    each as its own memoized `<ReactMarkdown>`; only the final block gets a new
    prop per frame and everything above it is a memo hit.

    Two facts make the rendered DOM provably identical rather than merely
    similar:

    1. react-markdown (9.x) renders its root's children into a `Fragment`, and
       a Fragment emits no DOM node — so N sibling instances inside `.markdown`
       produce exactly the same flat element list as one instance over the
       whole text. `:first-child`, `+`, `~` and `nth-child` all still resolve
       against the same real parent.
    2. The only combinators under `.markdown` in `themes/base.css` are
       `:not(pre) > code` and `li > p:first-child` / `li > p:last-child` — all
       scoped inside a single block, and the `li > p` pair sits inside a list,
       which this splitter never splits.

    One difference does exist, and it is inert: the whole-text render keeps the
    `\n` text nodes that separate top-level blocks in the source, and rendering
    each block on its own drops them. The assistant bubble is in normal
    white-space flow (the one `white-space: pre-wrap` in `base.css` is scoped
    to `.msg-user .bubble`, which renders plain text and never reaches this
    code), and in normal flow a whitespace-only text node between two
    block-level elements generates no boxes. `MarkdownBlocks.test.tsx`
    normalises exactly that whitespace and nothing else.

    That leaves one correctness question: is the partition semantically
    correct? Every rule below is therefore *conservative* — a boundary is
    rejected unless it is unambiguous, and anything we cannot bound the scope
    of bails the whole message back to single-block rendering (i.e. exactly the
    old behaviour). The worst case for a bug here is "split less than we could
    have", which costs performance, never correctness.

    `blocks.join('') === text` always holds. Verified against the whole-text
    render, over every prefix of a corpus, in `markdownSplit.test.ts`. */

const BLANK = /^[ \t]*$/;
/** Opening fence: ``` or ~~~ (3+), indented at most 3 spaces. */
const FENCE_OPEN = /^ {0,3}(`{3,}|~{3,})/;
/** Closing fence: same char, at least as long, nothing but whitespace after. */
const FENCE_CLOSE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/;
/** Bullet or ordered list marker followed by a space (or end of line). */
const LIST_MARKER = /^ {0,3}(?:[-*+]|\d{1,9}[.)])(?:[ \t]|$)/;
const BLOCKQUOTE = /^ {0,3}>/;
/** A line-initial `<` can open an HTML block whose end we do not track. */
const HTML_BLOCK_START = /^ {0,3}</;
/** Link reference / footnote definition — appears later, but changes earlier
    blocks, so its presence disables splitting for the whole message. */
const REF_DEF = /^ {0,3}\[[^\]\n]+\]:/m;

export type SplitCache = {
  /** The exact text this split was produced from. */
  text: string;
  /** Partition of `text`; `blocks.join('') === text`. */
  blocks: string[];
  /** Sticky: once a message bails, it renders whole-text for good. */
  bailed: boolean;
};

/** Strip the line ending so the per-line regexes can anchor with `$`. */
const body = (line: string) => line.replace(/\r?\n$/, '');

/** Would splitting at a blank run before `next` change how anything renders?

    - An indented (4+) following line is an indented code block, or the
      continuation of one — never a boundary.
    - Inside a blockquote followed by more blockquote: a blank line does end a
      blockquote in CommonMark, but this stays conservative rather than
      relying on that.
    - Inside a list, a blank line before another marker or an indented line is
      an *interior* blank — it makes the list loose (`<li>x</li>` becomes
      `<li><p>x</p></li>`), which is a real divergence, not a theoretical one.
      The list only closes when the next line is at column 0 and is not a
      marker. */
function isSafeBoundary(next: string, inList: boolean, inQuote: boolean): boolean {
  if (/^(?: {4,}|\t)/.test(next)) return false;
  if (inQuote && BLOCKQUOTE.test(next)) return false;
  if (inList) return !LIST_MARKER.test(next) && !/^[ \t]/.test(next);
  return true;
}

/** Partition `text` into independently-renderable top-level blocks.

    Returns `bailed: true` (and a single whole-text block) when the message
    contains something whose scope cannot be bounded — a link reference or
    footnote definition, or a line-initial `<` that may open an HTML block. */
export function scanBlocks(text: string): { blocks: string[]; bailed: boolean } {
  if (!text) return { blocks: [], bailed: false };
  if (REF_DEF.test(text)) return { blocks: [text], bailed: true };

  const lines = text.split(/(?<=\n)/);
  const blocks: string[] = [];
  let fence: { char: string; len: number } | null = null;
  let inList = false;
  let inQuote = false;
  let blockStart = 0;
  let i = 0;

  while (i < lines.length) {
    const line = body(lines[i]);

    // Fenced code first: nothing inside a fence is interpreted, including
    // blank lines, stray `---`, and line-initial `<`.
    if (fence) {
      const close = FENCE_CLOSE.exec(line);
      if (close && close[1][0] === fence.char && close[1].length >= fence.len) fence = null;
      i++;
      continue;
    }
    const open = FENCE_OPEN.exec(line);
    if (open) {
      fence = { char: open[1][0], len: open[1].length };
      i++;
      continue;
    }
    if (HTML_BLOCK_START.test(line)) return { blocks: [text], bailed: true };

    if (BLANK.test(line)) {
      let j = i;
      while (j < lines.length && BLANK.test(body(lines[j]))) j++;
      // Trailing blank lines close nothing and need no boundary.
      if (j >= lines.length) break;
      if (isSafeBoundary(body(lines[j]), inList, inQuote)) {
        blocks.push(lines.slice(blockStart, j).join(''));
        blockStart = j;
        inList = false;
        inQuote = false;
      }
      i = j;
      continue;
    }

    if (BLOCKQUOTE.test(line)) inQuote = true;
    // Thematic breaks like `- - -` also match LIST_MARKER. That only makes us
    // more conservative (the block stays open longer), never wrong.
    if (!inList && LIST_MARKER.test(line)) inList = true;
    i++;
  }

  blocks.push(lines.slice(blockStart).join(''));
  return { blocks, bailed: false };
}

/** Reference implementation: a cold, whole-text split. The incremental path
    below must agree with this for every input — see the prefix sweep in
    `markdownSplit.test.ts`. */
export function splitMarkdownBlocks(text: string): string[] {
  return scanBlocks(text).blocks;
}

/** Streaming-aware split.

    Each frame re-scans only the tail instead of the whole message, which keeps
    the scan itself off the O(n²) curve too, not just the markdown parse.

    How much of the tail is the subtle part. A boundary's safety depends on the
    parser state before the blank run and on *the first line after it* — and
    nothing later than that. So a boundary becomes final as soon as that
    following line is newline-terminated, and only the very last boundary in a
    growing message can still flip.

    It really can flip, which is why the last *two* blocks are re-scanned
    rather than the last one. Streaming `1. one / 2. two / <blank> / 3` takes a
    boundary before `3`, because a bare `3` is a paragraph. One character later
    the line reads `3.`, which is an ordered-list marker continuing the list
    above — the blank is now interior to the list, it makes the list loose, and
    the two blocks have to merge again. Re-scanning from the start of the
    second-to-last block re-derives that decision with the full line in hand;
    that block began at a boundary whose following line is complete, so it is
    itself final and safe to resume from.

    Falls back to a cold split whenever `text` is not an extension of the
    cached text (edits, regenerate, session replay). */
export function splitIncremental(prev: SplitCache | null, text: string): SplitCache {
  if (prev && prev.text === text) return prev;

  const isExtension = prev !== null && text.startsWith(prev.text);
  // Bailing is sticky: a reference definition partway through a message
  // invalidates the blocks above it as well.
  if (isExtension && prev.bailed) return { text, blocks: [text], bailed: true };

  if (isExtension && prev.blocks.length > 0) {
    // -2, not -1: see the doc comment above.
    const stable = prev.blocks.slice(0, -2);
    let stableLen = 0;
    for (const b of stable) stableLen += b.length;
    const rescanned = scanBlocks(text.slice(stableLen));
    if (rescanned.bailed) return { text, blocks: [text], bailed: true };
    return { text, blocks: [...stable, ...rescanned.blocks], bailed: false };
  }

  const cold = scanBlocks(text);
  return { text, blocks: cold.blocks, bailed: cold.bailed };
}
