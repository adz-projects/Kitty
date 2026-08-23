import { describe, expect, it, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import ReactMarkdown from 'react-markdown';
import { CORPUS } from './markdownSplit.test';
import {
  MarkdownBlocks,
  MARKDOWN_COMPONENTS,
  REMARK_PLUGINS,
  REHYPE_PLUGINS,
} from './MarkdownBlocks';

// `MarkdownBlocks` pulls in `ipc` only for the external-link click handler;
// stub it so the module graph doesn't reach for a Tauri window under vitest.
// `vi.mock` is hoisted above the imports above, so the stub is in place first.
vi.mock('@/lib/ipc', () => ({ ipc: { openPath: vi.fn() } }));

/** Collapse whitespace that sits *entirely between two tags*.

    The whole-text render keeps the `\n` text nodes that separate top-level
    blocks in the source; rendering each block on its own drops them. That is
    the one and only difference between the two, and it is inert: the
    assistant bubble (`.bubble.markdown`) is in normal white-space flow — the
    single `white-space: pre-wrap` in `themes/base.css` is scoped to
    `.msg-user .bubble`, which renders plain text and never reaches this
    component — and in normal flow a whitespace-only text node between two
    block-level elements generates no boxes at all.

    The pattern is deliberately narrow. It requires `>` immediately before the
    whitespace and `<` immediately after, so it cannot touch text content: a
    real difference inside a `<pre>` (`a\n</pre>` vs `a</pre>`) does not match
    and still fails the comparison. Fenced blocks are never split anyway. */
const collapseInterTagWhitespace = (html: string) => html.replace(/>\s+</g, '><');

/** What the renderer produced before item S, and must keep producing: one
    `<ReactMarkdown>` over the whole message text. */
function renderWhole(text: string): string {
  return collapseInterTagWhitespace(
    renderToStaticMarkup(
      <div className="bubble markdown">
        <ReactMarkdown
          remarkPlugins={REMARK_PLUGINS}
          rehypePlugins={REHYPE_PLUGINS}
          components={MARKDOWN_COMPONENTS}
        >
          {text}
        </ReactMarkdown>
      </div>
    )
  );
}

/** What it produces now: N memoized `<ReactMarkdown>` siblings, one per block. */
function renderSplit(text: string): string {
  return collapseInterTagWhitespace(
    renderToStaticMarkup(
      <div className="bubble markdown">
        <MarkdownBlocks text={text} />
      </div>
    )
  );
}

describe('MarkdownBlocks — differential against whole-text rendering', () => {
  it('produces identical markup for every corpus entry', () => {
    for (const [name, text] of Object.entries(CORPUS)) {
      expect(renderSplit(text), name).toBe(renderWhole(text));
    }
  });

  // The one that matters. Streaming renders every prefix of the message, and
  // prefix-only divergence — a boundary that is correct at length N and wrong
  // at N+1 — is exactly the bug class a static corpus test cannot see.
  it('produces identical markup for every prefix of every corpus entry', () => {
    for (const [name, text] of Object.entries(CORPUS)) {
      for (let i = 0; i <= text.length; i++) {
        const prefix = text.slice(0, i);
        expect(renderSplit(prefix), `${name} @ ${i}`).toBe(renderWhole(prefix));
      }
    }
  });

  it('produces identical markup for a realistic mixed assistant reply', () => {
    const reply = [
      "Here's what I found.\n",
      '\n',
      '## Summary\n',
      '\n',
      'The parser has **three** problems:\n',
      '\n',
      '1. It re-reads the buffer from index `0` each line.\n',
      '2. It allocates a fresh `Vec` per line.\n',
      '3. It never releases the tail.\n',
      '\n',
      'Fix for the first one:\n',
      '\n',
      '```rust\n',
      "let pos = memchr(b'\\n', &self.buf[self.cursor..]);\n",
      '\n',
      'self.cursor += pos.unwrap_or(0);\n',
      '```\n',
      '\n',
      '> Note that this changes the cursor semantics.\n',
      '\n',
      '| case | before | after |\n',
      '| --- | --- | --- |\n',
      '| 1 line | 1 scan | 1 scan |\n',
      '| n lines | n²/2 | n |\n',
      '\n',
      'That should do it.\n',
    ].join('');
    for (let i = 0; i <= reply.length; i++) {
      const prefix = reply.slice(0, i);
      expect(renderSplit(prefix), `reply @ ${i}`).toBe(renderWhole(prefix));
    }
  });
});
