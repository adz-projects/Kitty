import { describe, it, expect, vi, beforeEach } from 'vitest';

// `stackStore` keeps a module-level `subscribed` guard so `init()` is safe to
// call repeatedly without re-subscribing — that only resets on a fresh
// module import, so each test below resets modules and re-imports to get a
// true "cold start" instead of leaking subscription state across tests.
const getStackStatus = vi.fn();
const getStartupPhase = vi.fn();
const onStackStatus = vi.fn();
const onStartupPhase = vi.fn();

vi.mock('@/lib/ipc', () => ({
  ipc: { getStackStatus, getStartupPhase },
  onStackStatus: (...args: unknown[]) => onStackStatus(...args),
  onStartupPhase: (...args: unknown[]) => onStartupPhase(...args),
}));

beforeEach(() => {
  vi.resetModules();
  getStackStatus.mockReset().mockResolvedValue('ok');
  getStartupPhase.mockReset().mockResolvedValue('ready');
  onStackStatus.mockReset().mockResolvedValue(() => {});
  onStartupPhase.mockReset().mockResolvedValue(() => {});
});

describe('stackStore.init', () => {
  it('primes status and startupPhase from ipc', async () => {
    const { useStackStore } = await import('./stackStore');
    await useStackStore.getState().init();

    expect(useStackStore.getState().status).toBe('ok');
    expect(useStackStore.getState().startupPhase).toBe('ready');
  });

  it('subscribes to stack status and startup phase exactly once across repeated calls', async () => {
    const { useStackStore } = await import('./stackStore');
    await useStackStore.getState().init();
    await useStackStore.getState().init();
    await useStackStore.getState().init();

    expect(onStackStatus).toHaveBeenCalledTimes(1);
    expect(onStartupPhase).toHaveBeenCalledTimes(1);
  });

  it('leaves status/startupPhase at their defaults when the backend is not ready yet', async () => {
    getStackStatus.mockRejectedValue(new Error('not ready'));
    getStartupPhase.mockRejectedValue(new Error('not ready'));

    const { useStackStore } = await import('./stackStore');
    await expect(useStackStore.getState().init()).resolves.toBeUndefined();

    expect(useStackStore.getState().status).toBe('starting');
    expect(useStackStore.getState().startupPhase).toBe('spawning_backend');
  });

  it('applies stack status updates delivered via the subscription callback', async () => {
    const { useStackStore } = await import('./stackStore');
    await useStackStore.getState().init();

    const handler = onStackStatus.mock.calls[0][0] as (p: {
      status: string;
      detail: string | null;
    }) => void;
    handler({ status: 'local_model_missing', detail: 'not running' });

    expect(useStackStore.getState().status).toBe('local_model_missing');
    expect(useStackStore.getState().detail).toBe('not running');
  });

  it('retries the subscriptions on a later init when the first attempt fails', async () => {
    // A failed subscription must NOT leave `subscribed` set, or every later
    // init() would silently skip attaching listeners forever.
    onStackStatus.mockRejectedValueOnce(new Error('listener failed'));

    const { useStackStore } = await import('./stackStore');
    await useStackStore.getState().init();
    await useStackStore.getState().init();

    // First onStackStatus call (init #1) rejected before onStartupPhase ran;
    // init #2 successfully subscribed to both.
    expect(onStackStatus).toHaveBeenCalledTimes(2);
    expect(onStartupPhase).toHaveBeenCalledTimes(1);

    // A third init must not re-subscribe now that #2 succeeded.
    await useStackStore.getState().init();
    expect(onStackStatus).toHaveBeenCalledTimes(2);
    expect(onStartupPhase).toHaveBeenCalledTimes(1);
  });
});
