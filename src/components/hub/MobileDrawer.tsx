import { useEffect, useRef } from 'react';
import { useChatStore } from '@/stores/chatStore';
import { useMobileUiStore } from '@/stores/mobileUiStore';
import { useRouteStore } from '@/stores/routeStore';
import { usePanGesture } from '@/hooks/usePanGesture';
import { useBackDismiss } from '@/hooks/useBackDismiss';
import { canScrollHorizontally, settle } from '@/lib/gesture';
import { SessionList } from '@/components/sessions/SessionList';
import { SettingsGearIcon } from '@/components/icons/SettingsGearIcon';

/** Anything a rightward swipe starting inside should leave alone: other
    layers sitting over the chat, where a swipe means something else or
    nothing at all. */
const NO_OPEN_FROM =
  '.artifacts-sheet, .sheet-scrim, .msg-menu-backdrop, .modal-backdrop, .mode-popover';

const overflowX = (el: unknown) => getComputedStyle(el as Element).overflowX;

/** Android's slide-out menu: search + saved chats, with Settings pinned below.
 *
 * Replaces the bottom tab bar. Opened by the header's menu button or a
 * rightward swipe anywhere on the chat; closed by the scrim, a leftward swipe,
 * Back, or picking something in it.
 *
 * Swipe-anywhere rather than an edge swipe because Android's gesture
 * navigation claims both screen edges for Back — an edge-only open would
 * fight the system on every phone that uses it. The cost is that a swipe
 * inside something horizontally scrollable (a wide code block, a table) has
 * to scroll that instead, which `canScrollHorizontally` decides.
 *
 * Always mounted so opening and closing can animate; while closed it is
 * `inert`, so its search box and rows can't take focus from behind the chat.
 * During a drag the panel and scrim are moved by writing styles directly
 * rather than through React state — a state update per touchmove would
 * re-render the whole session list sixty times a second. */
export function MobileDrawer() {
  const open = useMobileUiStore((s) => s.drawerOpen);
  const setOpen = useMobileUiStore((s) => s.setDrawerOpen);
  const view = useRouteStore((s) => s.view);
  const goto = useRouteStore((s) => s.goto);
  const sessionId = useChatStore((s) => s.sessionId);

  const rootRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLElement>(null);
  const scrimRef = useRef<HTMLDivElement>(null);

  // Picking a chat or a route is the end of the menu's job. Keyed on the
  // result rather than wired into each control, so anything that navigates —
  // including a notification tap — also closes it.
  useEffect(() => {
    setOpen(false);
  }, [sessionId, view, setOpen]);

  useEffect(() => {
    if (panelRef.current) panelRef.current.inert = !open;
  }, [open]);

  useBackDismiss(open, () => setOpen(false));

  const applyDrag = (progress: number) => {
    const panel = panelRef.current;
    const scrim = scrimRef.current;
    if (!panel || !scrim) return;
    const width = panel.offsetWidth;
    rootRef.current?.classList.add('dragging');
    panel.style.transform = `translateX(${(progress - 1) * width}px)`;
    scrim.style.opacity = String(progress);
  };

  const endDrag = (next: 'open' | 'closed') => {
    rootRef.current?.classList.remove('dragging');
    if (panelRef.current) panelRef.current.style.transform = '';
    if (scrimRef.current) scrimRef.current.style.opacity = '';
    setOpen(next === 'open');
  };

  usePanGesture(window, {
    axis: 'x',
    enabled: open || view === 'chat',
    shouldStart: ({ target, dx }) => {
      const from = useMobileUiStore.getState().drawerOpen;
      if (from) {
        if (dx >= 0) return false;
        return !canScrollHorizontally(target, 'left', overflowX);
      }
      if (dx <= 0 || useRouteStore.getState().view !== 'chat') return false;
      if (target.closest(NO_OPEN_FROM)) return false;
      return !canScrollHorizontally(target, 'right', overflowX);
    },
    onMove: (delta) => {
      const width = panelRef.current?.offsetWidth || 1;
      const base = useMobileUiStore.getState().drawerOpen ? width : 0;
      applyDrag(Math.min(1, Math.max(0, (base + delta) / width)));
    },
    onEnd: (delta, velocity) => {
      const width = panelRef.current?.offsetWidth || 1;
      const from = useMobileUiStore.getState().drawerOpen ? 'open' : 'closed';
      endDrag(settle(from, delta, velocity, width));
    },
  });

  return (
    <div ref={rootRef} className={`mobile-drawer-root${open ? ' open' : ''}`}>
      <div ref={scrimRef} className="mobile-drawer-scrim" onClick={() => setOpen(false)} />
      <nav
        ref={panelRef}
        className="mobile-drawer"
        aria-label="Menu"
        aria-hidden={!open}
        onClick={(e) => {
          // A tap on a saved chat opens it; close now rather than after the
          // session finishes loading, which is when `sessionId` changes. Rows
          // in bulk-select mode toggle instead, so they keep the menu open.
          if ((e.target as Element).closest('.session-item:not(.selectable)')) setOpen(false);
        }}
      >
        {/* Only the list scrolls. The footer is a sibling of it, not a child,
            so Settings stays on screen however long the chat history gets. */}
        <SessionList />
        <div className="mobile-drawer-footer">
          <button onClick={() => goto('settings')}>
            <SettingsGearIcon />
            <span>Settings</span>
          </button>
        </div>
      </nav>
    </div>
  );
}
