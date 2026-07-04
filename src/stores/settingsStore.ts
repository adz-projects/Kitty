import { create } from 'zustand';
import { ipc } from '@/lib/ipc';
import type { Config } from '@/lib/types';

interface SettingsState {
  config: Config | null;
  load: () => Promise<void>;
  save: (config: Config) => Promise<void>;
}

export const useSettingsStore = create<SettingsState>((set) => ({
  config: null,
  load: async () => set({ config: await ipc.getConfig() }),
  save: async (config: Config) => {
    await ipc.setConfig(config);
    set({ config });
  },
}));
