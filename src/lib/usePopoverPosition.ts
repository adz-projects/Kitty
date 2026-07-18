import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties } from 'react';

const MARGIN = 4;

/** Keeps a `.mode-popover`-style dropdown fully within the viewport,
    regardless of where its trigger sits — flips above/below and left/right
    of the trigger as needed, instead of ever requiring the popover's own
    content to scroll (owner ask: "never require you to scroll to see all
    items"). `position: fixed` is used rather than the class's default
    `position: absolute` so a popover inside a scrollable ancestor (a message
    in `.message-list`, a row in `.session-items`) can't get clipped by that
    ancestor's own overflow bounds either — it escapes the whole ancestor
    chain, positioned purely relative to the viewport.

    Also closes the popover on any click/tap outside both the trigger and the
    popover itself (owner ask) — pass `onClose` (typically `() => setOpen(false)`).
    Uses `pointerdown` rather than `click` so it fires before whatever's under
    the popover gets its own click, and so it doesn't get caught by any
    `stopPropagation()` a consumer's own `onClick` handler calls (there's
    exactly one such case, `SessionKebabMenu`'s row-click guard, and it only
    stops the `click` event, not `pointerdown`).

    Usage: attach `triggerRef` to the button that opens the popover and
    `popoverRef` to the popover's own root element, and spread `style` onto
    that root. Position is (re)computed via `useLayoutEffect` — after the DOM
    reflects `open` but before the browser paints, so there's no visible
    flash of the wrong position. */
export function usePopoverPosition(open: boolean, onClose: () => void) {
  const triggerRef = useRef<HTMLElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [style, setStyle] = useState<CSSProperties>({});

  useLayoutEffect(() => {
    if (!open) return;
    const trigger = triggerRef.current;
    const popover = popoverRef.current;
    if (!trigger || !popover) return;

    const triggerRect = trigger.getBoundingClientRect();
    const popoverRect = popover.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    // `.mode-popover`'s own CSS class sets `top`/`right` as its default
    // (position: absolute) placement. Every axis below must be explicitly
    // set to either a real value or `'auto'` — leaving one unset doesn't
    // "not override" the class rule, it leaves the class's `top`/`right`
    // active *alongside* whichever of `bottom`/`left` this computes, and a
    // `position:fixed` element with both `top` and `bottom` set gets its
    // height *stretched between them* instead of sized to content (this was
    // the actual bug: opening "above" only ever set `bottom`, so the class's
    // `top: calc(100% + 4px)` remained active too, collapsing the popover to
    // a sliver between two contradictory edges).
    const next: CSSProperties = {
      position: 'fixed',
      top: 'auto',
      bottom: 'auto',
      left: 'auto',
      right: 'auto',
    };

    // Vertical: prefer opening below the trigger; flip above only when that
    // actually gives more room — not just "doesn't fully fit below", which
    // was also a bug (a popover taller than both the space above AND below
    // would still get flipped to "above" unconditionally and overflow the
    // top edge instead). Either way, `maxHeight` clamps it to whichever
    // space it actually got, so even a popover taller than the viewport
    // can never render partly off-screen — it scrolls internally as a last
    // resort instead (still fully "in the frame", just not every item
    // visible without scrolling in that degenerate case).
    const spaceBelow = vh - triggerRect.bottom - MARGIN;
    const spaceAbove = triggerRect.top - MARGIN;
    const fitsBelow = popoverRect.height <= spaceBelow;
    const openBelow = fitsBelow || spaceBelow >= spaceAbove;

    if (openBelow) {
      next.top = Math.max(MARGIN, triggerRect.bottom + MARGIN);
      next.maxHeight = Math.max(0, vh - MARGIN - (next.top as number));
    } else {
      next.bottom = Math.max(MARGIN, vh - triggerRect.top + MARGIN);
      next.maxHeight = Math.max(0, spaceAbove);
    }

    // Horizontal: prefer right-aligned to the trigger (matches the existing
    // `.mode-popover` default); flip to left-aligned if that would overflow
    // the left edge. `maxWidth` is the same last-resort clamp as above.
    if (triggerRect.right - popoverRect.width >= MARGIN) {
      next.right = Math.max(MARGIN, vw - triggerRect.right);
    } else {
      next.left = Math.max(MARGIN, triggerRect.left);
    }
    next.maxWidth = Math.max(0, vw - 2 * MARGIN);

    setStyle(next);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handlePointerDown = (e: PointerEvent) => {
      const target = e.target as Node;
      if (triggerRef.current?.contains(target)) return; // trigger's own onClick toggles it
      if (popoverRef.current?.contains(target)) return; // an item's own onClick closes it
      onClose();
    };
    document.addEventListener('pointerdown', handlePointerDown);
    return () => document.removeEventListener('pointerdown', handlePointerDown);
  }, [open, onClose]);

  return { triggerRef, popoverRef, style };
}
