// @vitest-environment happy-dom
import { describe, it, expect, beforeEach } from 'vitest';
import { readCachedModeInfo, writeCachedModeInfo, modeInfoCacheKey } from './chatStore';

/** Backs the fix for the effort/mode dropdown delay on a *brand-new* window's
    first-ever "New Chat": `newSession()`'s existing same-window carry-forward
    only helps the 2nd+ session in a window's lifetime, since there's nothing
    to carry forward before any session has ever been created there. This
    localStorage cache (shared across windows, same webview origin) covers
    that gap by seeding the last-known values for the active provider. */

describe('mode info cache', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('round-trips mode/availableModes/thinkingEffort for a provider id', () => {
    const info = {
      mode: 'auto',
      availableModes: [{ id: 'auto', name: 'Auto', description: 'test' }],
      thinkingEffort: { current: 'medium', options: [{ id: 'low', name: 'Low' }] } as never,
    };
    writeCachedModeInfo('provider-1', info);
    expect(readCachedModeInfo('provider-1')).toEqual(info);
  });

  it('returns null when nothing has been cached for a provider id', () => {
    expect(readCachedModeInfo('never-seen')).toBeNull();
  });

  it('keys entries per provider id — does not leak across providers', () => {
    writeCachedModeInfo('provider-a', { mode: 'auto', availableModes: [], thinkingEffort: null });
    writeCachedModeInfo('provider-b', { mode: 'manual', availableModes: [], thinkingEffort: null });
    expect(readCachedModeInfo('provider-a')?.mode).toBe('auto');
    expect(readCachedModeInfo('provider-b')?.mode).toBe('manual');
  });

  it('returns null (not a throw) for corrupted JSON in storage', () => {
    localStorage.setItem(modeInfoCacheKey('bad'), 'not json{{');
    expect(readCachedModeInfo('bad')).toBeNull();
  });

  it('a later write for the same provider id overwrites the earlier one', () => {
    writeCachedModeInfo('provider-1', { mode: 'auto', availableModes: [], thinkingEffort: null });
    writeCachedModeInfo('provider-1', { mode: 'manual', availableModes: [], thinkingEffort: null });
    expect(readCachedModeInfo('provider-1')?.mode).toBe('manual');
  });
});
