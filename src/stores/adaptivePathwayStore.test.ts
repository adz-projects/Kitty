import { describe, it, expect, vi, beforeEach } from 'vitest';

const getAdaptivePathwayMcpStatus = vi.fn();

vi.mock('@/lib/ipc', () => ({
  ipc: { getAdaptivePathwayMcpStatus },
}));

beforeEach(() => {
  vi.resetModules();
  getAdaptivePathwayMcpStatus.mockReset();
});

describe('adaptivePathwayStore.init', () => {
  it('is available when the pathway MCP server has registered tools', async () => {
    getAdaptivePathwayMcpStatus.mockResolvedValue({
      status: 'connected',
      error_message: null,
      tool_count: 2,
    });
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();
    expect(useAdaptivePathwayStore.getState().available).toBe(true);
  });

  it('is unavailable when the server is connected but has zero tools', async () => {
    getAdaptivePathwayMcpStatus.mockResolvedValue({
      status: 'connected',
      error_message: null,
      tool_count: 0,
    });
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();
    expect(useAdaptivePathwayStore.getState().available).toBe(false);
  });

  it('is unavailable when the status call returns null (engine disabled)', async () => {
    getAdaptivePathwayMcpStatus.mockResolvedValue(null);
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();
    expect(useAdaptivePathwayStore.getState().available).toBe(false);
  });

  it('leaves availability false when the backend call rejects', async () => {
    getAdaptivePathwayMcpStatus.mockRejectedValue(new Error('not ready'));
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await expect(useAdaptivePathwayStore.getState().init()).resolves.toBeUndefined();
    expect(useAdaptivePathwayStore.getState().available).toBe(false);
  });

  it('re-queries on every call rather than caching (no subscribe-once guard)', async () => {
    getAdaptivePathwayMcpStatus.mockResolvedValue({
      status: 'connected',
      error_message: null,
      tool_count: 2,
    });
    const { useAdaptivePathwayStore } = await import('./adaptivePathwayStore');
    await useAdaptivePathwayStore.getState().init();
    await useAdaptivePathwayStore.getState().init();
    expect(getAdaptivePathwayMcpStatus).toHaveBeenCalledTimes(2);
  });
});
