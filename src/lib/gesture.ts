// Pure gesture math for the Android shell's swipeable panels (the menu drawer
// and the artifacts sheet). Kept free of React and the DOM event plumbing so
// the decisions — "did that swipe open it?", "does this touch belong to a
// scroller?" — can be tested without simulating touches.

/** Fraction of the panel's size a drag has to cover to flip its state. */
export const SETTLE_DISTANCE_FRACTION = 0.35;
/** A flick this fast (px/ms) flips the state regardless of distance. */
export const SETTLE_FLING_VELOCITY = 0.5;

/** Decide where a released drag lands.
 *
 * `delta` is the signed displacement *toward open* (positive = more open) and
 * `velocity` is signed the same way. Starting from `from`, a drag that covers
 * enough distance or is flung hard enough in the other state's direction
 * flips it; anything short of that snaps back. A fling *against* the drag
 * direction wins over distance, since that is the user changing their mind. */
export function settle(
  from: 'open' | 'closed',
  delta: number,
  velocity: number,
  size: number
): 'open' | 'closed' {
  const threshold = size * SETTLE_DISTANCE_FRACTION;
  if (from === 'closed') {
    if (velocity <= -SETTLE_FLING_VELOCITY) return 'closed';
    if (velocity >= SETTLE_FLING_VELOCITY || delta >= threshold) return 'open';
    return 'closed';
  }
  if (velocity >= SETTLE_FLING_VELOCITY) return 'open';
  if (velocity <= -SETTLE_FLING_VELOCITY || -delta >= threshold) return 'closed';
  return 'open';
}

/** Minimal shape of an element this module inspects — lets tests pass plain
    objects instead of building a DOM. */
export interface ScrollableLike {
  scrollLeft: number;
  scrollWidth: number;
  clientWidth: number;
  scrollTop?: number;
  parentElement: ScrollableLike | null;
}

/** True when a horizontal swipe starting at `el` should scroll something
 * rather than move a panel: some ancestor (up to, not including, `stop`)
 * overflows horizontally and still has room to scroll in the direction the
 * content would move. `direction` is the finger's direction — a finger moving
 * right reveals content to the *left*, so it needs `scrollLeft > 0`.
 *
 * `overflowX` is injected so tests don't need `getComputedStyle`; callers in
 * the app pass the real one. Elements whose overflow is `visible`/`hidden`
 * can't be scrolled by a finger even if their content is wider. */
export function canScrollHorizontally(
  el: ScrollableLike | null,
  direction: 'left' | 'right',
  overflowX: (el: ScrollableLike) => string,
  stop: ScrollableLike | null = null
): boolean {
  for (let node = el; node && node !== stop; node = node.parentElement) {
    if (node.scrollWidth <= node.clientWidth + 1) continue;
    const ov = overflowX(node);
    if (ov !== 'auto' && ov !== 'scroll') continue;
    if (direction === 'right' && node.scrollLeft > 0) return true;
    if (direction === 'left' && node.scrollLeft + node.clientWidth < node.scrollWidth - 1) {
      return true;
    }
  }
  return false;
}
