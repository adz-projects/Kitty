// Kitty's right-click menu (after Bacon's). The browser's own menu never
// shows (on Windows the WebView2 bridge, `src-tauri/src/context_menu.rs`, suppresses it and
// passes on the native items worth keeping: spelling, Cut/Copy/Paste, Select
// all). Whether a menu appears, and what is in it, is decided here:
//
// * A surface registers a provider on its element (`registerContextMenu`,
//   or the `useContextMenu` hook). The closest provider above the
//   right-clicked element answers, with groups of items, or null for none.
// * With no provider, an editable field gets the native editing items and a
//   text selection gets Copy; everything else (chrome, buttons, empty space)
//   gets no menu at all.
//
// `resolveMenu` is pure and tested; `ContextMenu.tsx` draws the result.

/** A native WebView2 item, run by WebView2 itself when picked. */
export interface NativeItem {
  command_id: number;
  /** `spellCheck`, `addToDictionary`, `cut`, `copy`, `paste`,
      `pasteAndMatchStyle`, `selectAll`, `inspectElement`. */
  name: string;
  label: string;
  enabled: boolean;
}

/** What the WebView2 bridge reports about one right-click. */
export interface MenuRequest {
  editable: boolean;
  has_selection: boolean;
  items: NativeItem[];
}

export type MenuEntry =
  | {
      kind: 'item';
      label: string;
      run: () => void;
      shortcut?: string;
      disabled?: boolean;
      danger?: boolean;
    }
  | { kind: 'native'; commandId: number; label: string; shortcut?: string; disabled?: boolean };

/** What a surface offers: its groups (separated by rules), and whether the
    native editing items join them. */
export interface ProvidedMenu {
  groups: MenuEntry[][];
  /** Cut/Copy/Paste from the native menu, before the surface's groups.
      Default: when the target is editable or has a selection. */
  clipboard?: boolean;
  /** Select all, after the surface's groups. Default: as `clipboard`. */
  selectAll?: boolean;
  /** Spelling suggestions first. Default: true (they only exist on a
      misspelled word in an editable field). */
  spelling?: boolean;
}

export interface MenuContext {
  /** The right-clicked element. */
  target: Element;
  editable: boolean;
  hasSelection: boolean;
}

export type Provider = (ctx: MenuContext) => ProvidedMenu | null;

const SHORTCUTS: Record<string, string> = {
  cut: 'Ctrl+X',
  copy: 'Ctrl+C',
  paste: 'Ctrl+V',
  pasteAndMatchStyle: 'Ctrl+Shift+V',
  selectAll: 'Ctrl+A',
};

const LABELS: Record<string, string> = {
  cut: 'Cut',
  copy: 'Copy',
  paste: 'Paste',
  pasteAndMatchStyle: 'Paste as plain text',
  selectAll: 'Select all',
  addToDictionary: 'Add to dictionary',
  inspectElement: 'Inspect',
};

function native(item: NativeItem): MenuEntry {
  return {
    kind: 'native',
    commandId: item.command_id,
    // Kitty's own wording; spelling suggestions keep theirs (the word).
    label: LABELS[item.name] ?? item.label,
    shortcut: SHORTCUTS[item.name],
    disabled: !item.enabled,
  };
}

function pick(items: NativeItem[], names: string[]): MenuEntry[] {
  return names.flatMap((name) => items.filter((i) => i.name === name).map(native));
}

/** The menu for one right-click: groups without empty ones, or null when
    there is nothing to offer. */
export function resolveMenu(
  provided: ProvidedMenu | null,
  ctx: { editable: boolean; hasSelection: boolean },
  items: NativeItem[]
): MenuEntry[][] | null {
  const editing = ctx.editable || ctx.hasSelection;
  const menu: ProvidedMenu =
    provided ??
    // No surface: the editing items only, and only where there is something
    // to edit or copy.
    (editing ? { groups: [] } : { groups: [], clipboard: false, selectAll: false });
  const withClipboard = menu.clipboard ?? editing;
  const withSelectAll = menu.selectAll ?? editing;
  // In a read-only spot only Copy makes sense.
  const clipboardNames = ctx.editable ? ['cut', 'copy', 'paste', 'pasteAndMatchStyle'] : ['copy'];
  const groups: MenuEntry[][] = [
    menu.spelling === false ? [] : pick(items, ['spellCheck', 'addToDictionary']),
    withClipboard ? pick(items, clipboardNames) : [],
    ...menu.groups,
    withSelectAll ? pick(items, ['selectAll']) : [],
    pick(items, ['inspectElement']),
  ].filter((g) => g.length > 0);
  // The developer item alone is not a menu.
  const real = groups.flat().some((e) => e.kind === 'item' || e.label !== 'Inspect');
  return real ? groups : null;
}

// ------------------------------------------------------------- registry

const providers = new WeakMap<Element, Provider>();

/** Offer a menu for right-clicks inside `el`. Returns the unregister. */
export function registerContextMenu(el: Element, provider: Provider): () => void {
  providers.set(el, provider);
  return () => {
    if (providers.get(el) === provider) providers.delete(el);
  };
}

/** The closest provider at or above `target`. */
export function providerFor(target: Element | null): Provider | null {
  for (let el = target; el; el = el.parentElement) {
    const p = providers.get(el);
    if (p) return p;
  }
  return null;
}

/** Whether `el` is a field the user types in. */
export function isEditable(el: Element | null): boolean {
  if (!el) return false;
  if (el instanceof HTMLTextAreaElement) return !el.readOnly && !el.disabled;
  if (el instanceof HTMLInputElement) {
    const textual = ['text', 'search', 'url', 'email', 'tel', 'password', 'number', ''];
    return textual.includes(el.type) && !el.readOnly && !el.disabled;
  }
  return (el as HTMLElement).isContentEditable === true;
}

/** Ctrl+F, and nothing else held: the app's find (the browser's is off). */
export function isFindKey(
  e: Pick<KeyboardEvent, 'ctrlKey' | 'altKey' | 'shiftKey' | 'metaKey' | 'key'>
) {
  return e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey && e.key.toLowerCase() === 'f';
}

/** A file or folder Kitty shows (an output card, an artifact): Open, Show in
    folder, Copy path. `act` reports a failure where the card shows errors. */
export function fileMenu(act: {
  open: () => void;
  reveal: () => void;
  copyPath: () => void;
}): ProvidedMenu {
  return {
    groups: [
      [
        { kind: 'item', label: 'Open', run: act.open },
        { kind: 'item', label: 'Show in folder', run: act.reveal },
      ],
      [{ kind: 'item', label: 'Copy path', run: act.copyPath }],
    ],
  };
}
