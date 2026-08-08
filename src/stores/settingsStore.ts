import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import type { Config } from '@/lib/types';

interface SettingsState {
  config: Config | null;
  /** Last load failure message — a transient backend-unavailable rejection
      used to reject `load()` outright (config stuck null + unhandled promise
      rejection from `void load()` callers); now captured in state so a caller
      can retry, and the UI can show why Settings is blank. */
  loadError: string | null;
  load: () => Promise<void>;
  save: (config: Config) => Promise<void>;
}

export const useSettingsStore = create<SettingsState>((set) => ({
  config: null,
  loadError: null,
  load: async () => {
    try {
      set({ config: await ipc.getConfig(), loadError: null });
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
    }
  },
  save: async (config: Config) => {
    try {
      await ipc.setConfig(config);
      set({ config, loadError: null });
    } catch (e) {
      // Record the failure in state (callers using `void save()` shouldn't
      // leave Settings silently stale) but STILL rethrow — the Settings
      // window's save handler awaits this to surface the error to the user.
      set({ loadError: e instanceof Error ? e.message : String(e) });
      throw e;
    }
  },
}));
