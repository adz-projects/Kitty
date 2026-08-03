import { describe, it, expect, vi, beforeEach } from 'vitest';

// Same module-level "subscribed once" guard as stackStore.ts — reset modules
// per test for a true cold start rather than leaking subscription state.
const getAdaptivePathwayStatus = vi.fn();
const onAdaptivePathwayStatus = vi.fn();

vi.mock('@/lib/ipc', () => ({
  ipc: { getAdaptivePathwayStatus },
  onAdaptivePathwayStatus: (...args: unknown[]) => onAdaptivePathwayStatus(...args),
}));

beforeEach(() => {
  vi.resetModules();
  getAdaptivePathwayStatus.mockReset().mockResolvedValue('ok');
  onAdaptivePathwayStatus.mockReset().mockResolvedValue(() => {});
});

describe('adaptivePathwayStore.init', () => {
  it('primes status from ipc', async () => {
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();
    expect(useAdaptivePathwayStore.getState().status).toBe('ok');
  });

  it('subscribes to status updates exactly once across repeated init calls', async () => {
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();
    await useAdaptivePathwayStore.getState().init();

    expect(onAdaptivePathwayStatus).toHaveBeenCalledTimes(1);
  });

  it('leaves status at the disabled default when the backend is not ready yet', async () => {
    getAdaptivePathwayStatus.mockRejectedValue(new Error('not ready'));

    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await expect(useAdaptivePathwayStore.getState().init()).resolves.toBeUndefined();

    expect(useAdaptivePathwayStore.getState().status).toBe('disabled');
  });

  it('applies status updates delivered via the subscription callback', async () => {
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();

    const handler = onAdaptivePathwayStatus.mock.calls[0][0] as (p: { status: string }) => void;
    handler({ status: 'down' });

    expect(useAdaptivePathwayStore.getState().status).toBe('down');
  });
});
