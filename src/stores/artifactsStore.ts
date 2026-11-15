import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import { outputsDir, sortEntries } from '@/lib/outputs';
import type { FileEntry, OutputSnapshotEntry } from '@/lib/types';

/** The top-level entries of `<cwd>/outputs`. A directory that does not exist
    yet (nothing made so far) is simply empty; a failed read is too, which at
    worst hides a card until the next poll. */
export async function listOutputs(cwd: string): Promise<FileEntry[]> {
  try {
    return await ipc.listDirectory(outputsDir(cwd));
  } catch {
    return [];
  }
}

/** The recursive before/after signature of `<cwd>/outputs` for change detection
    (one entry per top-level file/folder, each aggregating its whole subtree).
    Unlike `listOutputs` this sees nested and sub-second edits, so any edit to a
    file in `outputs` counts as a new artifact. Only taken at turn boundaries;
    a missing folder or failed read is `[]`, same as `listOutputs`. */
export async function snapshotOutputs(cwd: string): Promise<OutputSnapshotEntry[]> {
  try {
    return await ipc.outputsSnapshot(outputsDir(cwd));
  } catch {
    return [];
  }
}

interface ArtifactsState {
  /** Whether the drawer (desktop) or sheet (Android) is open. Every launch
      starts closed, and nothing but the user opens it. */
  open: boolean;
  /** Something new landed in `outputs` while the drawer was shut: the title-bar
      button turns red until the drawer is opened. In memory only. */
  unseen: boolean;
  /** Whether this window has an artifacts pane at all (the hub does; the
      overlay does not). Output cards offer "Show in artifacts" only if so. */
  hasPane: boolean;
  /** The last listing of the outputs folder, and the `cwd` it belongs to — a
      listing for another chat's folder is never shown. */
  listingCwd: string | null;
  entries: FileEntry[];
  setOpen: (open: boolean) => void;
  toggle: () => void;
  /** Flag fresh output; has no effect while the drawer is open. */
  markNew: () => void;
  setHasPane: (hasPane: boolean) => void;
  /** Store a listing taken for `cwd`. */
  setListing: (cwd: string, entries: FileEntry[]) => void;
  /** List `cwd`'s outputs folder from disk into the store. */
  refresh: (cwd: string) => Promise<void>;
  /** The window moved to another chat. */
  resetForChat: () => void;
}

export const useArtifactsStore = create<ArtifactsState>((set, get) => ({
  open: false,
  unseen: false,
  hasPane: false,
  listingCwd: null,
  entries: [],
  setOpen: (open) => set(open ? { open: true, unseen: false } : { open: false }),
  toggle: () => get().setOpen(!get().open),
  markNew: () => {
    if (!get().open) set({ unseen: true });
  },
  setHasPane: (hasPane) => set({ hasPane }),
  setListing: (cwd, entries) => set({ listingCwd: cwd, entries: sortEntries(entries) }),
  refresh: async (cwd) => {
    const entries = await listOutputs(cwd);
    get().setListing(cwd, entries);
  },
  resetForChat: () => set({ unseen: false }),
}));

/** The listing for `cwd`, or `null` while none has been taken for it. */
export function listingFor(
  s: Pick<ArtifactsState, 'listingCwd' | 'entries'>,
  cwd: string | null
): FileEntry[] | null {
  return cwd && s.listingCwd === cwd ? s.entries : null;
}
