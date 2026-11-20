import { describe, expect, it } from 'vitest';
import { resolveMenu, type MenuEntry, type NativeItem } from './contextMenu';

let next = 1;
const item = (name: string, label = name, enabled = true): NativeItem => ({
  command_id: next++,
  name,
  label,
  enabled,
});

const editingItems = [
  item('cut', '&Cut'),
  item('copy'),
  item('paste'),
  item('pasteAndMatchStyle'),
  item('selectAll'),
];

const labels = (groups: MenuEntry[][] | null) => groups?.map((g) => g.map((e) => e.label)) ?? null;
const own = (label: string): MenuEntry => ({ kind: 'item', label, run: () => {} });

describe('resolveMenu', () => {
  it('offers nothing on chrome: no surface, nothing editable, nothing selected', () => {
    expect(resolveMenu(null, { editable: false, hasSelection: false }, editingItems)).toBeNull();
  });

  it('gives a plain field the editing items in Kitty’s words', () => {
    expect(
      labels(resolveMenu(null, { editable: true, hasSelection: false }, editingItems))
    ).toEqual([['Cut', 'Copy', 'Paste', 'Paste as plain text'], ['Select all']]);
  });

  it('gives a read-only selection Copy and Select all only', () => {
    expect(
      labels(resolveMenu(null, { editable: false, hasSelection: true }, editingItems))
    ).toEqual([['Copy'], ['Select all']]);
  });

  it('puts spelling first, then editing, the surface’s groups, then Select all', () => {
    const items = [item('spellCheck', 'receive'), item('addToDictionary'), ...editingItems];
    const groups = resolveMenu(
      { groups: [[own('Bold'), own('Italic')], [own('Comment')]] },
      { editable: true, hasSelection: true },
      items
    );
    expect(labels(groups)).toEqual([
      ['receive', 'Add to dictionary'],
      ['Cut', 'Copy', 'Paste', 'Paste as plain text'],
      ['Bold', 'Italic'],
      ['Comment'],
      ['Select all'],
    ]);
  });

  it('lets a surface leave the editing items out and drops empty groups', () => {
    const groups = resolveMenu(
      { groups: [[own('Open')], [], [own('Delete…')]], clipboard: false, selectAll: false },
      { editable: false, hasSelection: true },
      editingItems
    );
    expect(labels(groups)).toEqual([['Open'], ['Delete…']]);
  });

  it('a surface with nothing to offer and no editing items is no menu', () => {
    expect(
      resolveMenu({ groups: [] }, { editable: false, hasSelection: false }, editingItems)
    ).toBeNull();
  });

  it('never shows the developer item alone', () => {
    expect(
      resolveMenu(null, { editable: false, hasSelection: false }, [item('inspectElement')])
    ).toBeNull();
    const withRow = resolveMenu(
      { groups: [[own('Open')]] },
      { editable: false, hasSelection: false },
      [item('inspectElement')]
    );
    expect(labels(withRow)).toEqual([['Open'], ['Inspect']]);
  });

  it('keeps a disabled native item, greyed', () => {
    const groups = resolveMenu(null, { editable: true, hasSelection: false }, [
      item('paste', 'Paste', false),
    ]);
    expect(groups?.[0][0]).toMatchObject({ kind: 'native', label: 'Paste', disabled: true });
  });
});
