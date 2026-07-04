// The ONLY file that calls Tauri `invoke()` / `listen()`. Everything else in the
// frontend goes through these typed wrappers (CLAUDE.md rule 2).

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { Config, StackStatus, StackStatusPayload } from './types';

export const ipc = {
  getConfig: () => invoke<Config>('get_config'),
  setConfig: (config: Config) => invoke<void>('set_config', { config }),
  toggleOverlay: () => invoke<void>('toggle_overlay'),
  hideOverlay: () => invoke<void>('hide_overlay'),
  openSettings: (section?: string) => invoke<void>('open_settings', { section: section ?? null }),
  openMain: () => invoke<void>('open_main'),
  getStackStatus: () => invoke<StackStatus>('get_stack_status'),
  restartGoosed: () => invoke<void>('restart_goosed'),
};

/** Subscribe to stack status changes. Returns an unlisten fn. */
export function onStackStatus(cb: (payload: StackStatusPayload) => void): Promise<UnlistenFn> {
  return listen<StackStatusPayload>('stack://status', (e) => cb(e.payload));
}
