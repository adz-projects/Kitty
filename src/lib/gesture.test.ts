import { describe, expect, it } from 'vitest';
import { canScrollHorizontally, settle, type ScrollableLike } from './gesture';

describe('settle', () => {
  const size = 300;

  it('snaps back on a short, slow drag', () => {
    expect(settle('closed', 40, 0.1, size)).toBe('closed');
    expect(settle('open', -40, -0.1, size)).toBe('open');
  });

  it('flips once the drag covers enough of the panel', () => {
    expect(settle('closed', 120, 0, size)).toBe('open');
    expect(settle('open', -120, 0, size)).toBe('closed');
  });

  it('flips on a fast fling regardless of distance', () => {
    expect(settle('closed', 10, 0.9, size)).toBe('open');
    expect(settle('open', -10, -0.9, size)).toBe('closed');
  });

  it('lets a reverse fling override distance', () => {
    expect(settle('closed', 200, -0.9, size)).toBe('closed');
    expect(settle('open', -200, 0.9, size)).toBe('open');
  });
});

function node(
  props: Partial<ScrollableLike> & { overflow?: string },
  parent: ScrollableLike | null = null
): ScrollableLike & { overflow: string } {
  return {
    scrollLeft: 0,
    scrollWidth: 100,
    clientWidth: 100,
    overflow: 'visible',
    parentElement: parent,
    ...props,
  };
}
const overflowOf = (el: ScrollableLike) => (el as unknown as { overflow: string }).overflow;

describe('canScrollHorizontally', () => {
  it('is false when nothing overflows', () => {
    const root = node({});
    const leaf = node({}, root);
    expect(canScrollHorizontally(leaf, 'right', overflowOf)).toBe(false);
  });

  it('finds a scrolled code block up the tree for a rightward swipe', () => {
    const pre = node({ scrollWidth: 400, clientWidth: 100, scrollLeft: 50, overflow: 'auto' });
    const span = node({}, pre);
    expect(canScrollHorizontally(span, 'right', overflowOf)).toBe(true);
  });

  it('ignores a scroller already at its left edge for a rightward swipe', () => {
    const pre = node({ scrollWidth: 400, clientWidth: 100, scrollLeft: 0, overflow: 'auto' });
    expect(canScrollHorizontally(pre, 'right', overflowOf)).toBe(false);
    expect(canScrollHorizontally(pre, 'left', overflowOf)).toBe(true);
  });

  it('ignores overflow that a finger cannot scroll', () => {
    const clipped = node({
      scrollWidth: 400,
      clientWidth: 100,
      scrollLeft: 50,
      overflow: 'hidden',
    });
    expect(canScrollHorizontally(clipped, 'right', overflowOf)).toBe(false);
  });

  it('stops at the given ancestor', () => {
    const outer = node({ scrollWidth: 400, clientWidth: 100, scrollLeft: 50, overflow: 'auto' });
    const inner = node({}, outer);
    expect(canScrollHorizontally(inner, 'right', overflowOf, outer)).toBe(false);
  });
});
