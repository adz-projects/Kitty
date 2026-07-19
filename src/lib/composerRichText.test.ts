// @vitest-environment happy-dom
import { describe, it, expect } from 'vitest';
import { serializeComposerToMarkdown, headingClassName } from './composerRichText';

// These build DOM trees by hand to match exactly what Composer.tsx's live
// conversions produce, rather than driving real typing/execCommand (jsdom
// doesn't implement contentEditable editing behavior) — the serializer is
// the one part of this feature that's meaningfully unit-testable.

describe('serializeComposerToMarkdown', () => {
  it('serializes a bare text-node line (the first line, before any Enter)', () => {
    const root = document.createElement('div');
    root.appendChild(document.createTextNode('hello world'));
    expect(serializeComposerToMarkdown(root)).toBe('hello world');
  });

  it('serializes plain div lines joined by newlines', () => {
    const root = document.createElement('div');
    const l1 = document.createElement('div');
    l1.textContent = 'line one';
    const l2 = document.createElement('div');
    l2.textContent = 'line two';
    root.append(l1, l2);
    expect(serializeComposerToMarkdown(root)).toBe('line one\nline two');
  });

  it('serializes a heading block back to "#" syntax using data-level, not the clamped CSS class', () => {
    const root = document.createElement('div');
    const heading = document.createElement('div');
    heading.className = headingClassName(4); // clamps visually to level 3
    heading.dataset.level = '4';
    heading.textContent = 'My Title';
    root.appendChild(heading);
    expect(serializeComposerToMarkdown(root)).toBe('#### My Title');
    expect(heading.className).toContain('composer-heading-3');
  });

  it('serializes h1/h2/h3 with the right number of hashes', () => {
    for (const level of [1, 2, 3]) {
      const root = document.createElement('div');
      const heading = document.createElement('div');
      heading.className = headingClassName(level);
      heading.dataset.level = String(level);
      heading.textContent = 'T';
      root.appendChild(heading);
      expect(serializeComposerToMarkdown(root)).toBe(`${'#'.repeat(level)} T`);
    }
  });

  it('serializes a native <ul><li> list to "- " lines', () => {
    const root = document.createElement('div');
    const ul = document.createElement('ul');
    for (const text of ['first', 'second', 'third']) {
      const li = document.createElement('li');
      li.textContent = text;
      ul.appendChild(li);
    }
    root.appendChild(ul);
    expect(serializeComposerToMarkdown(root)).toBe('- first\n- second\n- third');
  });

  it("serializes a list nested inside its line's wrapper div — the actual shape `execCommand('insertUnorderedList')` produces (it wraps the block's own children in a new <ul> rather than replacing the block itself, confirmed interactively), not a bare top-level <ul>", () => {
    const root = document.createElement('div');
    const wrapper = document.createElement('div');
    wrapper.className = 'composer-block';
    const ul = document.createElement('ul');
    const li = document.createElement('li');
    li.textContent = 'first item';
    ul.appendChild(li);
    wrapper.appendChild(ul);
    root.appendChild(wrapper);
    expect(serializeComposerToMarkdown(root)).toBe('- first item');
  });

  it('mixes plain lines, a heading, and a list in document order', () => {
    const root = document.createElement('div');
    const intro = document.createElement('div');
    intro.textContent = 'intro line';
    const heading = document.createElement('div');
    heading.className = headingClassName(2);
    heading.dataset.level = '2';
    heading.textContent = 'Section';
    const ul = document.createElement('ul');
    const li = document.createElement('li');
    li.textContent = 'item';
    ul.appendChild(li);
    root.append(intro, heading, ul);
    expect(serializeComposerToMarkdown(root)).toBe('intro line\n## Section\n- item');
  });

  it('ignores a stray trailing <br> (common empty-editable artifact)', () => {
    const root = document.createElement('div');
    root.appendChild(document.createElement('br'));
    expect(serializeComposerToMarkdown(root)).toBe('');
  });
});
