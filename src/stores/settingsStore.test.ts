import { describe, it, expect, vi, beforeEach } from 'vitest';
import type { Config } from '@/lib/types';

vi.mock('@/lib/ipc', () => ({
  ipc: {
    getConfig: vi.fn(),
    setConfig: vi.fn(),
  },
}));

const { ipc } = await import('@/lib/ipc');
const { useSettingsStore } = await import('./settingsStore');

const fakeConfig = { adaptive_pathway_enabled: true } as unknown as Config;

beforeEach(() => {
  vi.clearAllMocks();
  useSettingsStore.setState({ config: null });
});

describe('settingsStore.load', () => {
  it('fetches the config via ipc and stores it', async () => {
    vi.mocked(ipc.getConfig).mockResolvedValue(fakeConfig);

    await useSettingsStore.getState().load();

    expect(ipc.getConfig).toHaveBeenCalled();
    expect(useSettingsStore.getState().config).toBe(fakeConfig);
  });
});

describe('settingsStore.save', () => {
  it('persists via ipc.setConfig and updates local state', async () => {
    await useSettingsStore.getState().save(fakeConfig);

    expect(ipc.setConfig).toHaveBeenCalledWith(fakeConfig);
    expect(useSettingsStore.getState().config).toBe(fakeConfig);
  });

  it('does not update local state if the ipc call fails', async () => {
    vi.mocked(ipc.setConfig).mockRejectedValue(new Error('write failed'));

    await expect(useSettingsStore.getState().save(fakeConfig)).rejects.toThrow('write failed');
    expect(useSettingsStore.getState().config).toBeNull();
  });
});
