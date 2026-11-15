import { describe, it, expect, vi, beforeEach } from 'vitest';
import type { FileEntry } from '@/lib/types';

const listDirectory = vi.fn();

vi.mock('@/lib/ipc', () => ({
  ipc: { listDirectory: (...args: unknown[]) => listDirectory(...args) },
}));

import { listingFor, listOutputs, useArtifactsStore } from './artifactsStore';

const entry = (name: string, modified = 1): FileEntry => ({
  name,
  path: `/c/outputs/${name}`,
  size: 1,
  modified,
  is_dir: false,
});

beforeEach(() => {
  listDirectory.mockReset();
  useArtifactsStore.setState({
    open: false,
    unseen: false,
    hasPane: false,
    listingCwd: null,
    entries: [],
  });
});

describe('open / unseen', () => {
  it('starts shut and not flagged', () => {
    const s = useArtifactsStore.getState();
    expect(s.open).toBe(false);
    expect(s.unseen).toBe(false);
  });

  it('flags new output only while the drawer is shut', () => {
    useArtifactsStore.getState().markNew();
    expect(useArtifactsStore.getState().unseen).toBe(true);

    useArtifactsStore.setState({ unseen: false, open: true });
    useArtifactsStore.getState().markNew();
    expect(useArtifactsStore.getState().unseen).toBe(false);
  });

  it('opening clears the flag; closing does not set it', () => {
    useArtifactsStore.getState().markNew();
    useArtifactsStore.getState().setOpen(true);
    expect(useArtifactsStore.getState()).toMatchObject({ open: true, unseen: false });
    useArtifactsStore.getState().setOpen(false);
    expect(useArtifactsStore.getState()).toMatchObject({ open: false, unseen: false });
  });

  it('toggle flips the drawer, and opening by toggle clears the flag too', () => {
    useArtifactsStore.getState().markNew();
    useArtifactsStore.getState().toggle();
    expect(useArtifactsStore.getState()).toMatchObject({ open: true, unseen: false });
    useArtifactsStore.getState().toggle();
    expect(useArtifactsStore.getState().open).toBe(false);
  });

  it('switching chats clears the flag but leaves the drawer as it was', () => {
    useArtifactsStore.setState({ open: true, unseen: true });
    useArtifactsStore.getState().resetForChat();
    expect(useArtifactsStore.getState()).toMatchObject({ open: true, unseen: false });
  });
});

describe('listing', () => {
  it('is tagged with its cwd, so another chat never sees it', () => {
    useArtifactsStore.getState().setListing('/c', [entry('a.txt')]);
    const s = useArtifactsStore.getState();
    expect(listingFor(s, '/c')?.map((e) => e.name)).toEqual(['a.txt']);
    expect(listingFor(s, '/other')).toBeNull();
    expect(listingFor(s, null)).toBeNull();
  });

  it('lists newest first', () => {
    useArtifactsStore.getState().setListing('/c', [entry('old', 1), entry('new', 9)]);
    expect(useArtifactsStore.getState().entries.map((e) => e.name)).toEqual(['new', 'old']);
  });

  it('refresh reads <cwd>/outputs from disk', async () => {
    listDirectory.mockResolvedValue([entry('r.docx')]);
    await useArtifactsStore.getState().refresh('/c');
    expect(listDirectory).toHaveBeenCalledWith('/c/outputs');
    expect(useArtifactsStore.getState().listingCwd).toBe('/c');
  });

  it('a missing outputs folder lists as empty rather than failing', async () => {
    listDirectory.mockRejectedValue(new Error('no such directory'));
    expect(await listOutputs('/c')).toEqual([]);
    await useArtifactsStore.getState().refresh('/c');
    expect(listingFor(useArtifactsStore.getState(), '/c')).toEqual([]);
  });
});
