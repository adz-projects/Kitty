import { describe, expect, it } from 'vitest';
import { scanBlocks, splitIncremental, splitMarkdownBlocks } from './markdownSplit';

/** Corpus of the constructs where a naive blank-line split goes wrong. Reused
    by the differential render test (`MarkdownBlocks.test.tsx`), which sweeps
    every prefix of each entry. */
export const CORPUS: Record<string, string> = {
  plainParagraphs: 'First paragraph.\n\nSecond paragraph.\n\nThird.\n',
  tightList: '- alpha\n- beta\n- gamma\n\nAfter the list.\n',
  looseList: '- alpha\n\n- beta\n\n- gamma\n\nAfter the list.\n',
  nestedList: '- alpha\n  - inner one\n  - inner two\n\n- beta\n\ntrailing paragraph\n',
  orderedList: '1. one\n2. two\n\n3. three\n\ndone\n',
  fenceWithBlankLines: 'Intro:\n\n```py\ndef f():\n\n    return 1\n```\n\nOutro.\n',
  fenceWithFakeRule: 'Intro:\n\n```\n---\n\n- not a list\n```\n\nOutro.\n',
  tildeFence: 'A\n\n~~~js\nlet x = 1;\n\nlet y = 2;\n~~~\n\nB\n',
  nestedFenceLonger: 'A\n\n````md\n```\ninner\n```\n\nstill inside\n````\n\nB\n',
  table: 'Before.\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\nAfter.\n',
  blockquote: '> quoted line one\n> quoted line two\n\n> a second quote\n\nplain\n',
  setextHeading: 'Section Title\n=============\n\nBody text.\n\nAnother\n---\n\nMore.\n',
  thematicBreak: 'Above.\n\n---\n\nBelow.\n',
  indentedCode: 'Example:\n\n    indented code\n\n    more indented code\n\nDone.\n',
  headingsAndCode: '# Title\n\nSome text with `inline`.\n\n## Sub\n\n```ts\nconst a: number = 1;\n```\n\nEnd.\n',
  refDefinition: 'See [the docs][d] for more.\n\nAnother paragraph.\n\n[d]: https://example.com\n',
  footnote: 'Text with a note.[^1]\n\nMore text.\n\n[^1]: The note body.\n',
  htmlBlock: 'Before.\n\n<div class="x">\n  raw html\n</div>\n\nAfter.\n',
  listThenCode: '- item with code:\n\n  ```sh\n  echo hi\n  ```\n\n- second item\n\nafter\n',
  crlf: 'First line.\r\n\r\nSecond line.\r\n',
};

/** The invariant everything else depends on. */
function expectLossless(text: string) {
  expect(splitMarkdownBlocks(text).join('')).toBe(text);
}

describe('scanBlocks — losslessness', () => {
  it('partitions every corpus entry without losing a byte', () => {
    for (const [name, text] of Object.entries(CORPUS)) {
      expect(splitMarkdownBlocks(text).join(''), name).toBe(text);
    }
  });

  it('holds for every prefix of every corpus entry', () => {
    for (const [name, text] of Object.entries(CORPUS)) {
      for (let i = 0; i <= text.length; i++) {
        const prefix = text.slice(0, i);
        expect(splitMarkdownBlocks(prefix).join(''), `${name} @ ${i}`).toBe(prefix);
      }
    }
  });

  it('handles empty and whitespace-only input', () => {
    expect(scanBlocks('')).toEqual({ blocks: [], bailed: false });
    expectLossless('\n\n\n');
    expectLossless('   ');
  });
});

describe('scanBlocks — boundaries it must take', () => {
  it('splits consecutive paragraphs', () => {
    expect(splitMarkdownBlocks('one\n\ntwo\n\nthree\n')).toEqual([
      'one\n\n',
      'two\n\n',
      'three\n',
    ]);
  });

  it('splits after a list closes', () => {
    expect(splitMarkdownBlocks('- a\n- b\n\nafter\n')).toEqual(['- a\n- b\n\n', 'after\n']);
  });

  it('splits around a thematic break', () => {
    expect(splitMarkdownBlocks('above\n\n---\n\nbelow\n')).toEqual([
      'above\n\n',
      '---\n\n',
      'below\n',
    ]);
  });

  it('splits after a closed fence', () => {
    expect(splitMarkdownBlocks('```\ncode\n```\n\nafter\n')).toEqual([
      '```\ncode\n```\n\n',
      'after\n',
    ]);
  });
});

describe('scanBlocks — boundaries it must refuse', () => {
  it('never splits inside a fenced block, even across blank lines', () => {
    const text = '```py\na = 1\n\nb = 2\n```\n\nafter\n';
    expect(splitMarkdownBlocks(text)).toEqual(['```py\na = 1\n\nb = 2\n```\n\n', 'after\n']);
  });

  it('never splits inside an unterminated fence', () => {
    // Mid-stream: the closing fence has not arrived yet.
    expect(splitMarkdownBlocks('intro\n\n```py\na = 1\n\nb = 2\n')).toEqual([
      'intro\n\n',
      '```py\na = 1\n\nb = 2\n',
    ]);
  });

  it('keeps a loose list together (a blank line between items changes it)', () => {
    expect(splitMarkdownBlocks('- a\n\n- b\n\nafter\n')).toEqual(['- a\n\n- b\n\n', 'after\n']);
  });

  it('keeps an interior blank inside a list item together', () => {
    expect(splitMarkdownBlocks('- a\n\n  continued\n\nafter\n')).toEqual([
      '- a\n\n  continued\n\n',
      'after\n',
    ]);
  });

  it('does not split before an indented code continuation', () => {
    expect(splitMarkdownBlocks('    code one\n\n    code two\n')).toEqual([
      '    code one\n\n    code two\n',
    ]);
  });

  it('does not split between consecutive blockquote runs', () => {
    expect(splitMarkdownBlocks('> one\n\n> two\n')).toEqual(['> one\n\n> two\n']);
  });

  it('does not split a setext heading from its underline', () => {
    const blocks = splitMarkdownBlocks('Title\n=====\n\nbody\n');
    expect(blocks).toEqual(['Title\n=====\n\n', 'body\n']);
  });

  it('closes a tilde fence only on a matching tilde run', () => {
    const text = '~~~\na\n```\nb\n\nc\n~~~\n\nafter\n';
    expect(splitMarkdownBlocks(text)).toEqual(['~~~\na\n```\nb\n\nc\n~~~\n\n', 'after\n']);
  });

  it('closes a long fence only on an equally long run', () => {
    const text = '````\n```\ninner\n```\n\nstill in\n````\n\nafter\n';
    expect(splitMarkdownBlocks(text)).toEqual(['````\n```\ninner\n```\n\nstill in\n````\n\n', 'after\n']);
  });
});

describe('scanBlocks — whole-message bail-outs', () => {
  it('bails on a link reference definition', () => {
    const text = 'See [d].\n\nMore.\n\n[d]: https://example.com\n';
    expect(scanBlocks(text)).toEqual({ blocks: [text], bailed: true });
  });

  it('bails on a footnote definition', () => {
    const text = 'Note.[^1]\n\nMore.\n\n[^1]: body\n';
    expect(scanBlocks(text)).toEqual({ blocks: [text], bailed: true });
  });

  it('bails on a line-initial HTML block', () => {
    const text = 'Before.\n\n<div>\nraw\n</div>\n\nAfter.\n';
    expect(scanBlocks(text)).toEqual({ blocks: [text], bailed: true });
  });

  it('does not bail on a `<` inside a fenced block', () => {
    const text = '```html\n<div>ok</div>\n```\n\nafter\n';
    expect(scanBlocks(text).bailed).toBe(false);
  });

  it('does not bail on a task list item', () => {
    expect(scanBlocks('- [ ] todo\n- [x] done\n').bailed).toBe(false);
  });
});

describe('splitIncremental', () => {
  it('agrees with the cold split for every prefix of every corpus entry', () => {
    for (const [name, text] of Object.entries(CORPUS)) {
      let cache = null as ReturnType<typeof splitIncremental> | null;
      for (let i = 0; i <= text.length; i++) {
        const prefix = text.slice(0, i);
        cache = splitIncremental(cache, prefix);
        expect(cache.blocks.join(''), `${name} @ ${i} lossless`).toBe(prefix);
        // A bailed message renders whole-text; otherwise the incremental
        // partition must match the reference implementation exactly.
        if (!cache.bailed) {
          expect(cache.blocks, `${name} @ ${i} partition`).toEqual(splitMarkdownBlocks(prefix));
        } else {
          expect(cache.blocks, `${name} @ ${i} bailed`).toEqual([prefix]);
        }
      }
    }
  });

  it('returns the identical cache object when the text is unchanged', () => {
    const first = splitIncremental(null, 'a\n\nb\n');
    expect(splitIncremental(first, 'a\n\nb\n')).toBe(first);
  });

  it('reuses settled blocks across an append', () => {
    const first = splitIncremental(null, 'one\n\ntwo');
    const second = splitIncremental(first, 'one\n\ntwo and more');
    // The settled first block keeps its exact string identity, which is what
    // lets `MarkdownBlock`'s memo skip it.
    expect(second.blocks[0]).toBe(first.blocks[0]);
  });

  it('re-merges a boundary that a later character invalidates', () => {
    // Regression, found by the prefix sweep above. A bare `3` after a blank
    // line is a paragraph, so the boundary is correct at this point...
    const grown = '1. one\n2. two\n\n3';
    const before = splitIncremental(null, grown);
    expect(before.blocks).toEqual(['1. one\n2. two\n\n', '3']);

    // ...but one character later it is an ordered-list marker continuing the
    // list above, the blank becomes interior (which makes the list loose), and
    // the two blocks have to merge again.
    const after = splitIncremental(before, grown + '.');
    expect(after.blocks).toEqual([grown + '.']);
    expect(after.blocks).toEqual(splitMarkdownBlocks(grown + '.'));
  });

  it('falls back to a cold split when the text is not an extension', () => {
    const first = splitIncremental(null, 'one\n\ntwo\n');
    const replaced = splitIncremental(first, 'completely different\n\ntext\n');
    expect(replaced.blocks).toEqual(splitMarkdownBlocks('completely different\n\ntext\n'));
  });

  it('bails stickily once a reference definition arrives mid-stream', () => {
    let cache = splitIncremental(null, 'See [d].\n\nMore.\n');
    expect(cache.bailed).toBe(false);
    expect(cache.blocks.length).toBeGreaterThan(1);

    cache = splitIncremental(cache, 'See [d].\n\nMore.\n\n[d]: https://example.com\n');
    expect(cache.bailed).toBe(true);
    expect(cache.blocks).toEqual(['See [d].\n\nMore.\n\n[d]: https://example.com\n']);

    // Sticky: further appends stay whole-text.
    cache = splitIncremental(cache, 'See [d].\n\nMore.\n\n[d]: https://example.com\n\ntail\n');
    expect(cache.bailed).toBe(true);
    expect(cache.blocks.length).toBe(1);
  });
});
