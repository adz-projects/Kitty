// @vitest-environment jsdom
import { describe, it, expect } from 'vitest';
import { findOffsets, findRanges } from './findInPage';

describe('findOffsets', () => {
  it('finds every case-insensitive occurrence, without overlaps', () => {
    expect(findOffsets('Cat cat CAT', 'cat')).toEqual([0, 4, 8]);
    expect(findOffsets('aaaa', 'aa')).toEqual([0, 2]);
    expect(findOffsets('abc', '')).toEqual([]);
    expect(findOffsets('abc', 'x')).toEqual([]);
  });
});

describe('findRanges', () => {
  it('searches message text but not controls', () => {
    const root = document.createElement('div');
    root.innerHTML =
      '<div class="msg"><p>Kitty finds <b>kitty</b> text.</p><button>Kitty</button></div>';
    const ranges = findRanges(root, 'kitty');
    expect(ranges.map((r) => r.toString())).toEqual(['Kitty', 'kitty']);
    expect(findRanges(root, '   ')).toEqual([]);
  });
});
