import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from 'react';
import { createPortal } from 'react-dom';
import { create } from 'zustand';
import { ipc, onContextMenuRequested } from '@/lib/ipc';
import {
  isEditable,
  providerFor,
  registerContextMenu,
  resolveMenu,
  type MenuEntry,
  type NativeItem,
  type Provider,
} from '@/lib/contextMenu';
import { isAndroid } from '@/lib/platform';

interface OpenMenu {
  x: number;
  y: number;
  groups: MenuEntry[][];
  /** A WebView2 request is held open until answered (Windows). */
  pending: boolean;
}

interface MenuState {
  menu: OpenMenu | null;
  show: (menu: OpenMenu) => void;
  /** Close; a held request is answered with "nothing picked". */
  close: () => void;
}

export const useContextMenuStore = create<MenuState>((set, get) => ({
  menu: null,
  show: (menu) => {
    const previous = get().menu;
    // A menu already open is replaced; its own request was already answered
    // by the bridge when the new one came in.
    if (previous?.pending && !menu.pending) void ipc.contextMenuChoose(null).catch(() => {});
    set({ menu });
  },
  close: () => {
    const menu = get().menu;
    if (!menu) return;
    set({ menu: null });
    if (menu.pending) void ipc.contextMenuChoose(null).catch(() => {});
  },
}));

/** Offer a right-click menu inside `ref`'s element. The latest `provider` is
    always used (it may close over component state). */
export function useContextMenu(ref: RefObject<Element | null>, provider: Provider) {
  const latest = useRef(provider);
  latest.current = provider;
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    return registerContextMenu(el, (ctx) => latest.current(ctx));
  }, [ref]);
}

/** Where to open a menu raised from the keyboard (Shift+F10 / Menu key):
    at the text selection, else at the focused element. */
function keyboardPoint(target: Element): { x: number; y: number } {
  const sel = window.getSelection();
  if (sel && sel.rangeCount > 0) {
    const r = sel.getRangeAt(0).getBoundingClientRect();
    if (r.width || r.height) return { x: r.left, y: r.bottom };
  }
  const r = target.getBoundingClientRect();
  return { x: r.left + Math.min(24, r.width / 2), y: r.top + Math.min(r.height, 24) };
}

/** The window's one right-click menu, mounted once in App. It never takes
    focus: the editor keeps its caret and selection, so a native item (Paste,
    a spelling fix) acts where the user clicked. The keyboard drives it
    through a window listener instead. */
export function ContextMenu() {
  const menu = useContextMenuStore((s) => s.menu);
  const close = useContextMenuStore((s) => s.close);
  const [active, setActive] = useState(-1);
  const [place, setPlace] = useState<{ left: number; top: number } | null>(null);
  const panelRef = useRef<HTMLDivElement>(null);

  // Listen for right-clicks.
  useEffect(() => {
    if (isAndroid()) return; // long-press stays the system's
    const windows = /windows/i.test(navigator.userAgent);
    let last: { target: Element; x: number; y: number } | null = null;

    const open = (
      target: Element,
      at: { x: number; y: number },
      req: {
        editable: boolean;
        hasSelection: boolean;
        items: NativeItem[];
        pending: boolean;
      }
    ) => {
      const ctx = { target, editable: req.editable, hasSelection: req.hasSelection };
      const provided = providerFor(target)?.(ctx) ?? null;
      const groups = resolveMenu(provided, ctx, req.items);
      if (!groups) {
        if (req.pending) void ipc.contextMenuChoose(null).catch(() => {});
        useContextMenuStore.setState({ menu: null });
        return;
      }
      useContextMenuStore.getState().show({ ...at, groups, pending: req.pending });
    };

    const onDom = (e: MouseEvent) => {
      const target = (e.target as Element | null) ?? document.activeElement ?? document.body;
      const fromKeyboard = e.button !== 2 && e.clientX === 0 && e.clientY === 0;
      const at = fromKeyboard ? keyboardPoint(target) : { x: e.clientX, y: e.clientY };
      last = { target, ...at };
      if (windows) return; // the bridge asks next, with the native items
      // Elsewhere: no native items; Kitty's own menu straight away.
      e.preventDefault();
      const selected = (window.getSelection()?.toString() ?? '') !== '';
      open(target, at, {
        editable: isEditable(target),
        hasSelection: selected,
        items: [],
        pending: false,
      });
    };
    document.addEventListener('contextmenu', onDom, true);

    const unlisten = windows
      ? onContextMenuRequested((req) => {
          const target = last?.target ?? document.activeElement ?? document.body;
          const at = last ? { x: last.x, y: last.y } : keyboardPoint(target);
          last = null;
          open(target, at, {
            editable: req.editable,
            hasSelection: req.has_selection,
            items: req.items,
            pending: true,
          });
        })
      : null;
    return () => {
      document.removeEventListener('contextmenu', onDom, true);
      void unlisten?.then((fn) => fn());
    };
  }, []);

  const entries = menu?.groups.flat() ?? [];

  const run = (entry: MenuEntry) => {
    if (entry.disabled) return;
    if (entry.kind === 'native') {
      // WebView2 runs it, where the user right-clicked.
      useContextMenuStore.setState({ menu: null });
      void ipc.contextMenuChoose(entry.commandId).catch(() => {});
    } else {
      close();
      entry.run();
    }
  };

  // Place inside the window, flipped when it would run off an edge.
  useLayoutEffect(() => {
    setActive(-1);
    if (!menu) {
      setPlace(null);
      return;
    }
    const el = panelRef.current;
    const w = el?.offsetWidth ?? 0;
    const h = el?.offsetHeight ?? 0;
    const left = menu.x + w + 4 > window.innerWidth ? Math.max(4, menu.x - w) : menu.x;
    const top = menu.y + h + 4 > window.innerHeight ? Math.max(4, menu.y - h) : menu.y;
    setPlace({ left, top });
  }, [menu]);

  // Keyboard, outside clicks, scrolling and losing the window close it.
  useEffect(() => {
    if (!menu) return;
    const enabled = entries.map((e, i) => (e.disabled ? -1 : i)).filter((i) => i >= 0);
    const onKey = (e: KeyboardEvent) => {
      const move = (delta: number) => {
        if (enabled.length === 0) return;
        const at = enabled.indexOf(active);
        const next =
          at < 0
            ? delta > 0
              ? 0
              : enabled.length - 1
            : (at + delta + enabled.length) % enabled.length;
        setActive(enabled[next]);
      };
      const handled = () => {
        e.preventDefault();
        e.stopPropagation();
      };
      if (e.key === 'ArrowDown') {
        handled();
        move(1);
      } else if (e.key === 'ArrowUp') {
        handled();
        move(-1);
      } else if (e.key === 'Home') {
        handled();
        setActive(enabled[0] ?? -1);
      } else if (e.key === 'End') {
        handled();
        setActive(enabled[enabled.length - 1] ?? -1);
      } else if (e.key === 'Enter' || e.key === ' ') {
        handled();
        if (active >= 0) run(entries[active]);
        else close();
      } else if (e.key === 'Escape') {
        handled();
        close();
      } else if (!['Shift', 'Control', 'Alt', 'Meta'].includes(e.key)) {
        // Any other key: the menu goes and the key does what it does.
        close();
      }
    };
    const onDown = (e: MouseEvent) => {
      if (!panelRef.current?.contains(e.target as Node)) close();
    };
    window.addEventListener('keydown', onKey, true);
    window.addEventListener('mousedown', onDown, true);
    window.addEventListener('blur', close);
    window.addEventListener('resize', close);
    document.addEventListener('scroll', close, true);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      window.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('blur', close);
      window.removeEventListener('resize', close);
      document.removeEventListener('scroll', close, true);
    };
    // `run` and `entries` follow `menu`; `active` changes the key handling.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [menu, active, close]);

  if (!menu) return null;
  let index = -1;
  return createPortal(
    <div
      ref={panelRef}
      className="context-menu"
      role="menu"
      aria-label="Actions"
      aria-activedescendant={active >= 0 ? `ctx-item-${active}` : undefined}
      style={{
        left: place?.left ?? menu.x,
        top: place?.top ?? menu.y,
        visibility: place ? 'visible' : 'hidden',
      }}
      // Clicks never move focus out of the editor or field.
      onMouseDown={(e) => e.preventDefault()}
      onContextMenu={(e) => e.preventDefault()}
    >
      {menu.groups.map((group, g) => (
        <div key={g} className="context-menu-group" role="group">
          {group.map((entry) => {
            index += 1;
            const i = index;
            return (
              <button
                key={i}
                id={`ctx-item-${i}`}
                type="button"
                role="menuitem"
                tabIndex={-1}
                aria-disabled={entry.disabled || undefined}
                className={[
                  i === active ? 'active' : '',
                  entry.kind === 'item' && entry.danger ? 'danger-item' : '',
                ]
                  .filter(Boolean)
                  .join(' ')}
                onMouseEnter={() => setActive(entry.disabled ? -1 : i)}
                onClick={() => run(entry)}
              >
                <span className="context-menu-label">{entry.label}</span>
                {entry.shortcut && <span className="context-menu-keys">{entry.shortcut}</span>}
              </button>
            );
          })}
        </div>
      ))}
    </div>,
    document.body
  );
}
