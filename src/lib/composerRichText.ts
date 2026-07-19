// Live markdown auto-formatting for the composer's contentEditable input:
// "* " at the start of a line converts to a real bullet (native list, so
// Enter continues/exits it for free — see Composer.tsx's keydown handler);
// "#"-through-"######" + space converts the current line into a bold,
// size-stepped heading block. These are the DOM-touching pieces, kept
// separate from Composer.tsx so the parts that don't need a live browser
// (serialization) stay unit-testable.

/** Turns the composer's contentEditable DOM into the markdown-flavored plain
    text actually sent to goosed/matched against recipes — headings and
    bullets are visual-only inside the box; this is where they get translated
    back into the "# "/"- " syntax the model and the message-list renderer
    already understand. */
function pushList(list: Element, lines: string[]): void {
  for (const li of Array.from(list.children)) {
    if (li.tagName === 'LI') lines.push(`- ${li.textContent ?? ''}`);
  }
}

export function serializeComposerToMarkdown(root: Element): string {
  const lines: string[] = [];
  for (const node of Array.from(root.childNodes)) {
    if (node.nodeType === Node.TEXT_NODE) {
      lines.push(node.textContent ?? '');
      continue;
    }
    if (!(node instanceof Element)) continue;
    if (node.tagName === 'BR') continue;
    if (node.tagName === 'UL' || node.tagName === 'OL') {
      pushList(node, lines);
      continue;
    }
    const level = node.getAttribute('data-level');
    if (level) {
      lines.push(`${'#'.repeat(Number(level))} ${node.textContent ?? ''}`);
      continue;
    }
    // `execCommand('insertUnorderedList')` wraps the *current block's own
    // children* in a new list rather than replacing the block itself, so a
    // converted line is a wrapper div containing a <ul> — not a top-level
    // <ul> — one level down from what a fresh line looks like.
    const nestedList = Array.from(node.children).find(
      (c) => c.tagName === 'UL' || c.tagName === 'OL'
    );
    if (nestedList) {
      pushList(nestedList, lines);
      continue;
    }
    lines.push(node.textContent ?? '');
  }
  return lines.join('\n');
}

/** CSS classes for a heading block at a given real markdown level (1-6).
    Visual size clamps at level 3 per the owner spec (###  through ######
    all render the same 12px, bold) — the real level is kept separately via
    a `data-level` attribute so serialization can still emit the right
    number of `#`s. */
export function headingClassName(level: number): string {
  return `composer-block composer-heading composer-heading-${Math.min(level, 3)}`;
}

/** The current line's text from its block-start up to the caret, and the
    block element itself — used to detect the "*, then space" / "#.. then
    space" triggers. Returns null when there's no collapsed caret inside
    `root` (a range selection, or focus elsewhere). */
export function currentLineTextBeforeCaret(
  root: HTMLElement
): { text: string; blockEl: Element } | null {
  const sel = window.getSelection();
  if (!sel || sel.rangeCount === 0 || !sel.isCollapsed) return null;
  const range = sel.getRangeAt(0);
  if (!root.contains(range.startContainer)) return null;

  let blockEl: Node = range.startContainer;
  while (blockEl.parentNode && blockEl.parentNode !== root) {
    blockEl = blockEl.parentNode;
  }
  if (!(blockEl instanceof Element)) return null;

  const preRange = document.createRange();
  preRange.selectNodeContents(blockEl);
  preRange.setEnd(range.startContainer, range.startOffset);
  return { text: preRange.toString(), blockEl };
}

/** Walks up from the current selection to the nearest ancestor (within
    `root`) matching `predicate` — used to detect "caret is inside a list
    item" / "caret is inside a heading block" for Enter-key handling. */
export function findAncestor(
  root: HTMLElement,
  predicate: (el: Element) => boolean
): Element | null {
  const sel = window.getSelection();
  if (!sel || sel.rangeCount === 0) return null;
  let node: Node | null = sel.getRangeAt(0).startContainer;
  while (node && node !== root) {
    if (node instanceof Element && predicate(node)) return node;
    node = node.parentNode;
  }
  return null;
}

/** Deletes the `count` characters immediately before the caret — strips the
    trigger characters ("*", "#".repeat(n)) once a conversion fires, since
    the space that completed the trigger is intercepted and never inserted. */
export function deleteCharsBeforeCaret(count: number): void {
  const sel = window.getSelection();
  if (!sel || sel.rangeCount === 0) return;
  const range = sel.getRangeAt(0);
  const start = Math.max(0, range.startOffset - count);
  const delRange = range.cloneRange();
  delRange.setStart(range.startContainer, start);
  delRange.deleteContents();
}

/** Collapses the selection to the start of `el`'s contents. */
export function placeCaretAtStart(el: Element): void {
  const sel = window.getSelection();
  if (!sel) return;
  const range = document.createRange();
  range.selectNodeContents(el);
  range.collapse(true);
  sel.removeAllRanges();
  sel.addRange(range);
}

/** Collapses the selection to the end of `el`'s contents. */
export function placeCaretAtEnd(el: Element): void {
  const sel = window.getSelection();
  if (!sel) return;
  const range = document.createRange();
  range.selectNodeContents(el);
  range.collapse(false);
  sel.removeAllRanges();
  sel.addRange(range);
}
